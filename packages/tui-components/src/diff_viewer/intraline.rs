//! Intra-line changed ranges for a removed/added line pair.

use unicode_segmentation::UnicodeSegmentation;

use super::{ChangedRange, MAX_INTRALINE_LINE_GRAPHEMES, MAX_LCS_CELLS, lcs_table};

/// Changed byte ranges within a removed line and its paired added line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntralineChanges {
    /// Changed ranges of the removed (old) line, sorted and disjoint.
    pub old: Vec<ChangedRange>,
    /// Changed ranges of the added (new) line, sorted and disjoint.
    pub new: Vec<ChangedRange>,
}

/// Compute changed ranges for a removed line `old` and the added line `new`
/// that replaced it.
///
/// Lines are split into tokens (runs of word characters, runs of whitespace,
/// and single other graphemes) and compared with the longest common
/// subsequence used for line diffs. Unmatched tokens become changed ranges;
/// ranges separated only by whitespace are merged. When the lines share no
/// non-whitespace token, or the comparison would exceed the LCS budget, the
/// result is one range per side covering everything between the common
/// grapheme prefix and suffix. Ranges always lie on grapheme boundaries.
///
/// Identical lines produce no ranges. Returns `None` when either line has more
/// than 2,000 graphemes, so callers can skip emphasis for very long lines.
#[must_use]
pub fn intraline_changed_ranges(old: &str, new: &str) -> Option<IntralineChanges> {
    let old_tokens = tokens(old)?;
    let new_tokens = tokens(new)?;
    let prefix = old_tokens
        .iter()
        .zip(&new_tokens)
        .take_while(|(old, new)| old.text == new.text)
        .count();
    let suffix = old_tokens[prefix..]
        .iter()
        .rev()
        .zip(new_tokens[prefix..].iter().rev())
        .take_while(|(old, new)| old.text == new.text)
        .count();
    let old_middle = &old_tokens[prefix..old_tokens.len() - suffix];
    let new_middle = &new_tokens[prefix..new_tokens.len() - suffix];
    if old_middle.is_empty() && new_middle.is_empty() {
        return Some(IntralineChanges::default());
    }
    if old_middle.is_empty() || new_middle.is_empty() {
        return Some(span_changes(old_middle, new_middle, old, new));
    }
    if old_middle.len().saturating_mul(new_middle.len()) > MAX_LCS_CELLS {
        return Some(span_changes(old_middle, new_middle, old, new));
    }
    let old_texts = old_middle
        .iter()
        .map(|token| token.text)
        .collect::<Vec<_>>();
    let new_texts = new_middle
        .iter()
        .map(|token| token.text)
        .collect::<Vec<_>>();
    let table = lcs_table(&old_texts, &new_texts);
    let mut old_changed = vec![true; old_middle.len()];
    let mut new_changed = vec![true; new_middle.len()];
    let mut matched_content = false;
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old_middle.len() && new_index < new_middle.len() {
        if old_texts[old_index] == new_texts[new_index] {
            old_changed[old_index] = false;
            new_changed[new_index] = false;
            matched_content |= !old_middle[old_index].whitespace;
            old_index += 1;
            new_index += 1;
        } else if table[old_index + 1][new_index] >= table[old_index][new_index + 1] {
            old_index += 1;
        } else {
            new_index += 1;
        }
    }
    if !matched_content {
        return Some(span_changes(old_middle, new_middle, old, new));
    }
    Some(IntralineChanges {
        old: merged_ranges(old_middle, &old_changed),
        new: merged_ranges(new_middle, &new_changed),
    })
}

struct Token<'a> {
    text: &'a str,
    start: usize,
    end: usize,
    whitespace: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenClass {
    Word,
    Space,
    Other,
}

fn class(grapheme: &str) -> TokenClass {
    let first = grapheme.chars().next().unwrap_or(' ');
    if first.is_whitespace() {
        TokenClass::Space
    } else if first.is_alphanumeric() || first == '_' {
        TokenClass::Word
    } else {
        TokenClass::Other
    }
}

fn tokens(text: &str) -> Option<Vec<Token<'_>>> {
    let mut output: Vec<Token<'_>> = Vec::new();
    let mut previous: Option<TokenClass> = None;
    let mut count = 0_usize;
    for (start, grapheme) in text.grapheme_indices(true) {
        count += 1;
        if count > MAX_INTRALINE_LINE_GRAPHEMES {
            return None;
        }
        let end = start + grapheme.len();
        let current = class(grapheme);
        let extends = previous == Some(current) && current != TokenClass::Other;
        match output.last_mut() {
            Some(last) if extends => {
                last.end = end;
                last.text = &text[last.start..end];
            }
            _ => output.push(Token {
                text: grapheme,
                start,
                end,
                whitespace: current == TokenClass::Space,
            }),
        }
        previous = Some(current);
    }
    Some(output)
}

/// One range per non-empty side covering the whole changed middle.
fn span_changes(
    old_middle: &[Token<'_>],
    new_middle: &[Token<'_>],
    old: &str,
    new: &str,
) -> IntralineChanges {
    let span = |middle: &[Token<'_>], text: &str| {
        let (Some(first), Some(last)) = (middle.first(), middle.last()) else {
            return Vec::new();
        };
        debug_assert!(text.is_char_boundary(first.start) && text.is_char_boundary(last.end));
        vec![ChangedRange::new(first.start, last.end)]
    };
    IntralineChanges {
        old: span(old_middle, old),
        new: span(new_middle, new),
    }
}

/// Merge changed tokens into ranges, bridging gaps made only of whitespace.
fn merged_ranges(tokens: &[Token<'_>], changed: &[bool]) -> Vec<ChangedRange> {
    let mut output: Vec<ChangedRange> = Vec::new();
    let mut bridge_from: Option<usize> = None;
    for (token, &is_changed) in tokens.iter().zip(changed) {
        if is_changed {
            match (output.last_mut(), bridge_from) {
                (Some(last), Some(_)) => last.end = token.end,
                _ => output.push(ChangedRange::new(token.start, token.end)),
            }
            bridge_from = Some(token.end);
        } else if !token.whitespace {
            bridge_from = None;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slices<'a>(text: &'a str, ranges: &[ChangedRange]) -> Vec<&'a str> {
        ranges
            .iter()
            .map(|range| &text[range.start..range.end])
            .collect()
    }

    #[test]
    fn identical_lines_have_no_changes() {
        assert_eq!(
            intraline_changed_ranges("same 東京", "same 東京"),
            Some(IntralineChanges::default())
        );
    }

    #[test]
    fn separate_edits_produce_separate_ranges() {
        let old = "call(alpha, beta, gamma)";
        let new = "call(alpha, delta, gamma2)";
        let changes = intraline_changed_ranges(old, new).expect("bounded");
        assert_eq!(slices(old, &changes.old), ["beta", "gamma"]);
        assert_eq!(slices(new, &changes.new), ["delta", "gamma2"]);
    }

    #[test]
    fn edits_separated_only_by_whitespace_merge() {
        let old = "let a = 1;";
        let new = "let b c = 1;";
        let changes = intraline_changed_ranges(old, new).expect("bounded");
        assert_eq!(slices(old, &changes.old), ["a"]);
        assert_eq!(slices(new, &changes.new), ["b c"]);
    }

    #[test]
    fn insertions_and_deletions_mark_only_one_side() {
        let changes = intraline_changed_ranges("foo()", "foo(bar)").expect("bounded");
        assert_eq!(changes.old, []);
        assert_eq!(slices("foo(bar)", &changes.new), ["bar"]);
    }

    #[test]
    fn unrelated_lines_mark_the_whole_changed_middle() {
        let old = "alpha beta";
        let new = "gamma delta";
        let changes = intraline_changed_ranges(old, new).expect("bounded");
        assert_eq!(slices(old, &changes.old), ["alpha beta"]);
        assert_eq!(slices(new, &changes.new), ["gamma delta"]);
    }

    #[test]
    fn ranges_lie_on_grapheme_boundaries() {
        for (old, new) in [
            ("face = \"😀\";", "face = \"😃\";"),
            ("東京都 天気", "東京都 晴れ"),
            ("cafe\u{301} x", "cafe\u{302} x"),
            ("👨‍👩‍👧‍👦 y", "👨‍👩‍👧 y"),
        ] {
            let changes = intraline_changed_ranges(old, new).expect("bounded");
            assert!(
                !changes.old.is_empty() && !changes.new.is_empty(),
                "{old:?}"
            );
            for (text, ranges) in [(old, &changes.old), (new, &changes.new)] {
                let boundaries = text
                    .grapheme_indices(true)
                    .map(|(start, _)| start)
                    .chain([text.len()])
                    .collect::<Vec<_>>();
                for range in ranges {
                    assert!(boundaries.contains(&range.start), "{text:?} {range:?}");
                    assert!(boundaries.contains(&range.end), "{text:?} {range:?}");
                }
            }
        }
        let changes = intraline_changed_ranges("cafe\u{301} x", "cafe\u{302} x").unwrap();
        assert_eq!(slices("cafe\u{301} x", &changes.old), ["cafe\u{301}"]);
    }

    #[test]
    fn very_long_lines_are_declined() {
        let long = "x".repeat(MAX_INTRALINE_LINE_GRAPHEMES + 1);
        assert_eq!(intraline_changed_ranges(&long, "x"), None);
        assert_eq!(intraline_changed_ranges("x", &long), None);
    }

    #[test]
    fn oversized_token_comparisons_fall_back_to_one_range() {
        let old = (0..400)
            .map(|index| format!("a{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let new = (0..400)
            .map(|index| format!("b{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let changes = intraline_changed_ranges(&old, &new).expect("bounded");
        assert_eq!(changes.old.len(), 1);
        assert_eq!(changes.new.len(), 1);
    }
}
