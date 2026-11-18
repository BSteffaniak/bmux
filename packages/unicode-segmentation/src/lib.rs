//! Allocation-free Unicode 17 extended grapheme cluster segmentation (UAX #29).

#![no_std]

mod engine;
mod properties;

use core::iter::FusedIterator;
use engine::RiRun;

/// Iterates over extended grapheme clusters, borrowing slices of `text`.
pub fn graphemes(text: &str) -> Graphemes<'_> {
    Graphemes(grapheme_indices(text))
}

/// Iterates over clusters and their starting byte offsets in the original string.
pub fn grapheme_indices(text: &str) -> GraphemeIndices<'_> {
    GraphemeIndices {
        text,
        front: 0,
        back: text.len(),
        reverse_ri: RiRun::default(),
    }
}

/// Double-ended iterator returned by [`grapheme_indices`].
#[derive(Clone, Debug)]
pub struct GraphemeIndices<'a> {
    text: &'a str,
    front: usize,
    back: usize,
    reverse_ri: RiRun,
}

impl<'a> Iterator for GraphemeIndices<'a> {
    type Item = (usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.front;
        let length = engine::first_len(&self.text[start..self.back])?;
        self.front += length;
        Some((start, &self.text[start..self.front]))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (
            usize::from(self.front < self.back),
            Some(self.back - self.front),
        )
    }
}

impl DoubleEndedIterator for GraphemeIndices<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let end = self.back;
        self.back = engine::last_start(self.text, self.front, end, &mut self.reverse_ri)?;
        Some((self.back, &self.text[self.back..end]))
    }
}

impl FusedIterator for GraphemeIndices<'_> {}

/// Double-ended iterator returned by [`graphemes`].
#[derive(Clone, Debug)]
pub struct Graphemes<'a>(GraphemeIndices<'a>);

impl<'a> Iterator for Graphemes<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(_, cluster)| cluster)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl DoubleEndedIterator for Graphemes<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|(_, cluster)| cluster)
    }
}

impl FusedIterator for Graphemes<'_> {}

#[cfg(test)]
mod tests {
    use super::grapheme_indices;

    #[test]
    fn cached_ri_run_survives_front_consumption_and_clone() {
        let mut iter = grapheme_indices("\u{600}🇦🇧🇨🇩🇪🇫🇬🇭🇮\u{301}");
        assert_eq!(iter.next_back(), Some((34, "🇮\u{301}")));
        assert_eq!(iter.next(), Some((0, "\u{600}🇦🇧")));
        let mut fork = iter.clone();
        assert_eq!(iter.next_back(), Some((26, "🇬🇭")));
        assert_eq!(iter.next(), Some((10, "🇨🇩")));
        assert_eq!(iter.next_back(), Some((18, "🇪🇫")));
        assert_eq!(iter.next(), None);
        assert_eq!(fork.next(), Some((10, "🇨🇩")));
        assert_eq!(fork.next(), Some((18, "🇪🇫")));
        assert_eq!(fork.next_back(), Some((26, "🇬🇭")));
        assert_eq!(fork.next_back(), None);
    }
}
