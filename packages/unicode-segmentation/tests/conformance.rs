use bmux_unicode_segmentation::{grapheme_indices, graphemes};

// Offsets are relative to this particular input, even when it is a subslice.
fn check(text: &str, starts: &[usize]) {
    for order in 0..4 {
        let mut gs = graphemes(text);
        let mut indices = grapheme_indices(text);
        let (mut left, mut right) = (0, starts.len());
        for step in 0..starts.len() {
            let back = match order {
                0 => false,
                1 => true,
                2 => step % 2 == 0,
                _ => step % 3 == 2,
            };
            // Consume a clone from the opposite end before continuing with the original.
            let (mut cg, mut ci) = (gs.clone(), indices.clone());
            let (g, i, k) = if back {
                (cg.next(), ci.next(), left)
            } else {
                (cg.next_back(), ci.next_back(), right - 1)
            };
            check_item(text, starts, k, g, i);
            let (g, i, k) = if back {
                right -= 1;
                (gs.next_back(), indices.next_back(), right)
            } else {
                left += 1;
                (gs.next(), indices.next(), left - 1)
            };
            check_item(text, starts, k, g, i);
        }
        assert_eq!((gs.next(), gs.next_back()), (None, None), "{text:?}");
        assert_eq!(
            (indices.next_back(), indices.next()),
            (None, None),
            "{text:?}"
        );
    }
}

fn check_item(text: &str, starts: &[usize], k: usize, g: Option<&str>, i: Option<(usize, &str)>) {
    let start = starts[k];
    let expected = &text[start..starts.get(k + 1).copied().unwrap_or(text.len())];
    assert_eq!(g, Some(expected), "{text:?} cluster {k}");
    assert_eq!(i, Some((start, expected)), "{text:?} cluster {k}");
    assert_eq!(g.unwrap().as_ptr(), expected.as_ptr());
    assert_eq!(i.unwrap().1.as_ptr(), expected.as_ptr());
}

fn scalar(hex: &str) -> char {
    char::from_u32(u32::from_str_radix(hex, 16).unwrap()).unwrap()
}

fn rows(data: &str) -> impl Iterator<Item = &str> {
    data.lines()
        .map(|s| s.split('#').next().unwrap().trim())
        .filter(|s| !s.is_empty())
}

#[test]
fn unicode_17_grapheme_break_test() {
    let mut count = 0;
    for row in rows(include_str!("fixtures/GraphemeBreakTest.txt")) {
        let mut text = String::new();
        let mut breaks = Vec::new();
        for token in row.split_whitespace() {
            match token {
                "÷" => breaks.push(text.len()),
                "×" => (),
                hex => text.push(scalar(hex)),
            }
        }
        assert_eq!(breaks.pop(), Some(text.len()));
        check(&text, &breaks);
        count += 1;
    }
    assert_eq!(count, 766);
}

#[test]
fn unicode_17_emoji_sequences() {
    let mut count = 0;
    for row in rows(include_str!("fixtures/emoji-test.txt")) {
        let (codes, _) = row.split_once(';').unwrap();
        let text: String = codes.split_whitespace().map(scalar).collect();
        check(&text, &[0]);
        count += 1;
    }
    assert_eq!(count, 5225);
}

#[test]
fn frozen_oracle_corpus() {
    let mut count = 0;
    for row in rows(include_str!("fixtures/oracle-1.13.3.txt")) {
        let (codes, offsets) = row.split_once(';').unwrap();
        let text: String = codes.split_whitespace().map(scalar).collect();
        let starts: Vec<_> = offsets
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        check(&text, &starts);
        count += 1;
    }
    assert_eq!(count, 4096);
}

#[test]
fn upstream_extended_regressions() {
    // Adapted from unicode-segmentation 1.13.3 tests/test.rs (licenses in fixtures/).
    check(" \u{600}\u{600} ", &[0, 1]);
    check("\u{600}  ", &[0, 3]);
    check("👨‍👧‍👦", &[0]);
    check("🤸🏾🏾", &[0]);
    check("a̐éö̲\r\n", &[0, 3, 6, 11]);
    check("\n\r\n\r", &[0, 1, 3]);
}

#[test]
fn long_context_and_subslices() {
    let flags = "🇦".repeat(513);
    for skip in [0, 4] {
        let suffix = &flags[skip..];
        check(suffix, &(0..suffix.len()).step_by(8).collect::<Vec<_>>());
    }
    let marks = "\u{301}".repeat(1024);
    let emoji = format!("🚀{marks}\u{200d}🚀!");
    check(&emoji, &[0, emoji.len() - 1]);
    check(&emoji[4..], &[0, emoji.len() - 9, emoji.len() - 5]);
    let indic = format!("क{marks}्{marks}क");
    check(&indic, &[0]);
    check(&indic[3..], &[0, indic.len() - 6]);
    check(&indic[..indic.len() - 3], &[0]);
    check("", &[]);
}
