use crate::properties::lookup;

const CR: u8 = 1;
const LF: u8 = 2;
const CONTROL: u8 = 3;
const EXTEND: u8 = 4;
const ZWJ: u8 = 5;
const RI: u8 = 6;
const PREPEND: u8 = 7;
const SPACING_MARK: u8 = 8;
const L: u8 = 9;
const V: u8 = 10;
const T: u8 = 11;
const LV: u8 = 12;
const LVT: u8 = 13;
const EP: u8 = 0x10;
const CONSONANT: u8 = 1;
const INCB_EXTEND: u8 = 2;
const LINKER: u8 = 3;

fn gcb(properties: u8) -> u8 {
    properties & 0x0f
}

fn incb(properties: u8) -> u8 {
    (properties >> 5) & 3
}

/// Known RI bytes [start, end). Absolute positions keep the cache valid when
/// next() consumes a pair from the other end: removing two preserves parity.
#[derive(Clone, Debug, Default)]
pub(super) struct RiRun {
    start: usize,
    end: usize,
}

impl RiRun {
    fn odd_prefix(&mut self, prefix: &str, at: usize) -> bool {
        // All Unicode 17 regional indicators (U+1F1E6..U+1F1FF) are four bytes.
        const WIDTH: usize = 4;
        if at <= self.start || at > self.end {
            let count = prefix
                .chars()
                .rev()
                .take_while(|&ch| gcb(lookup(ch)) == RI)
                .count();
            self.start = at - count * WIDTH;
            self.end = at;
        }
        ((at - self.start) / WIDTH) & 1 != 0
    }
}

// GB9c: C (Extend|Linker)* Linker (Extend|Linker)* × C. Successive
// consonants delimit these lookbehinds, so their interiors do not overlap.
fn linked_consonant(prefix: &str) -> bool {
    let mut linked = false;
    for ch in prefix.chars().rev() {
        match incb(lookup(ch)) {
            INCB_EXTEND => (),
            LINKER => linked = true,
            CONSONANT => return linked,
            _ => return false,
        }
    }
    false
}

// GB11: EP Extend* ZWJ × EP. Each triggering ZWJ delimits a distinct
// Extend run; even unsuccessful lookbehinds cost linear time in aggregate.
fn emoji_before_zwj(prefix: &str) -> bool {
    prefix
        .chars()
        .rev()
        .skip(1)
        .map(lookup)
        .find(|&p| gcb(p) != EXTEND)
        .is_some_and(|p| p & EP != 0)
}

// `prefix` ends with the scalar carrying `left`; `at` is the byte offset of
// `right` in the cache's coordinate system. Apply UAX #29 in rule order.
fn joined(left: u8, right: u8, prefix: &str, at: usize, ri: &mut RiRun) -> bool {
    match (gcb(left), gcb(right)) {
        (CR, LF) => true,                                                  // GB3
        (CR..=CONTROL, _) | (_, CR..=CONTROL) => false,                    // GB4/5
        (L, L | V | LV | LVT) | (LV | V, V | T) | (LVT | T, T) => true,    // GB6–8
        (_, EXTEND | ZWJ | SPACING_MARK) | (PREPEND, _) => true,           // GB9/9a/9b
        _ if incb(right) == CONSONANT && linked_consonant(prefix) => true, // GB9c
        (ZWJ, _) if right & EP != 0 => emoji_before_zwj(prefix),           // GB11
        (RI, RI) => ri.odd_prefix(prefix, at),                             // GB12/13
        _ => false,                                                        // GB999
    }
}

pub(super) fn first_len(text: &str) -> Option<usize> {
    let mut chars = text.char_indices();
    let mut left = lookup(chars.next()?.1);
    // Starting at a cluster boundary, at most two adjacent RIs can be
    // consumed. No run discovery outside the current cluster is needed.
    let mut ri = RiRun::default();
    for (at, ch) in chars {
        let right = lookup(ch);
        if !joined(left, right, &text[..at], at, &mut ri) {
            return Some(at);
        }
        left = right;
    }
    Some(text.len())
}

pub(super) fn last_start(text: &str, front: usize, end: usize, ri: &mut RiRun) -> Option<usize> {
    let remaining = &text[front..end];
    let mut chars = remaining.char_indices().rev();
    let (mut offset, last) = chars.next()?;
    let mut right = lookup(last);
    for (previous, ch) in chars {
        let left = lookup(ch);
        let at = front + offset;
        if !joined(left, right, &remaining[..offset], at, ri) {
            return Some(at);
        }
        offset = previous;
        right = left;
    }
    Some(front)
}
