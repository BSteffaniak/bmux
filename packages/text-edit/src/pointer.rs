//! Renderer-neutral pointer-gesture text selection.
//!
//! Hosts translate pointer cells to byte indices with their own layout, then
//! feed presses through [`ClickTracker`] (multi-click counting) and
//! [`PointerSelection`] (anchored press/drag selection over a
//! [`TextEditBuffer`]).

use std::ops::Range;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use unicode_segmentation::UnicodeSegmentation;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::{SelectionMode, TextEditBuffer, TextMotion};

/// Selection unit chosen by click count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SelectionGranularity {
    /// Select by grapheme/cell hit target.
    Character,
    /// Select whole words.
    Word,
    /// Select whole hard (`'\n'`-delimited) lines, including the newline.
    Line,
    /// Select the entire buffer.
    All,
}

/// Click sequence for single-field text inputs: 1 = character, 2 = word, 3 = all.
pub const FIELD_CLICK_GRANULARITIES: &[SelectionGranularity] = &[
    SelectionGranularity::Character,
    SelectionGranularity::Word,
    SelectionGranularity::All,
];

/// Click sequence for multi-line editors: 1 = character, 2 = word, 3 = line, 4 = all.
pub const EDITOR_CLICK_GRANULARITIES: &[SelectionGranularity] = &[
    SelectionGranularity::Character,
    SelectionGranularity::Word,
    SelectionGranularity::Line,
    SelectionGranularity::All,
];

/// Map a 1-based click count onto `sequence`.
///
/// Counts past the end of the sequence use its last entry; an empty sequence
/// (or a zero count) selects by character.
#[must_use]
pub const fn granularity_for_click_count(
    sequence: &[SelectionGranularity],
    count: u8,
) -> SelectionGranularity {
    if count == 0 || sequence.is_empty() {
        return SelectionGranularity::Character;
    }
    let index = count as usize - 1;
    if index < sequence.len() {
        sequence[index]
    } else {
        sequence[sequence.len() - 1]
    }
}

/// Multi-click detection policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClickPolicy {
    /// Maximum delay between consecutive presses of one multi-click.
    pub window: Duration,
    /// Maximum cell distance (per axis) between consecutive presses.
    pub distance: u16,
    /// Highest count reported; further presses stay at this count.
    /// Values are clamped to 1 through [`ClickPolicy::MAX_COUNT`].
    pub max_count: u8,
}

impl ClickPolicy {
    /// Default multi-click window.
    pub const DEFAULT_WINDOW: Duration = Duration::from_millis(500);
    /// Default multi-click cell distance.
    pub const DEFAULT_DISTANCE: u16 = 2;
    /// Hard upper bound on reported click counts.
    pub const MAX_COUNT: u8 = 8;

    /// Default policy counting up to `max_count` clicks.
    #[must_use]
    pub const fn with_max_count(max_count: u8) -> Self {
        Self {
            window: Self::DEFAULT_WINDOW,
            distance: Self::DEFAULT_DISTANCE,
            max_count,
        }
    }

    const fn effective_max_count(&self) -> u8 {
        if self.max_count == 0 {
            1
        } else if self.max_count > Self::MAX_COUNT {
            Self::MAX_COUNT
        } else {
            self.max_count
        }
    }
}

impl Default for ClickPolicy {
    fn default() -> Self {
        Self::with_max_count(3)
    }
}

/// The most recent press seen by a [`ClickTracker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClickRecord {
    /// Pointer cell column.
    pub x: u16,
    /// Pointer cell row.
    pub y: u16,
    /// Press time.
    pub at: Instant,
    /// Click count reported for the press.
    pub count: u8,
}

/// Counts consecutive nearby presses (single, double, triple, ... click).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ClickTracker {
    last: Option<ClickRecord>,
}

impl ClickTracker {
    /// Create a tracker with no click history.
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Recreate a tracker from a previously captured [`ClickTracker::last_click`].
    #[must_use]
    pub const fn from_last_click(last: Option<ClickRecord>) -> Self {
        Self { last }
    }

    /// Register a press at cell (`x`, `y`) at `now`.
    ///
    /// Returns `1..=policy.max_count`; the count resets to 1 when the press is
    /// farther than `policy.distance` or later than `policy.window` from the
    /// previous press.
    pub fn register(&mut self, x: u16, y: u16, now: Instant, policy: &ClickPolicy) -> u8 {
        let count = self.last.map_or(1, |last| {
            let near =
                last.x.abs_diff(x) <= policy.distance && last.y.abs_diff(y) <= policy.distance;
            let quick = now.saturating_duration_since(last.at) <= policy.window;
            if near && quick {
                last.count.saturating_add(1)
            } else {
                1
            }
        });
        let count = count.min(policy.effective_max_count());
        self.last = Some(ClickRecord {
            x,
            y,
            at: now,
            count,
        });
        count
    }

    /// Forget click history so the next press counts as a single click.
    pub const fn reset(&mut self) {
        self.last = None;
    }

    /// The most recent press, if any.
    #[must_use]
    pub const fn last_click(&self) -> Option<ClickRecord> {
        self.last
    }
}

/// Anchored pointer selection state machine.
///
/// [`begin`](Self::begin) selects the unit under the press (the anchor unit);
/// [`drag`](Self::drag) keeps that unit selected while extending at the same
/// granularity toward the pointer in either direction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PointerSelection {
    granularity: Option<SelectionGranularity>,
    anchor: Range<usize>,
}

impl PointerSelection {
    /// Create an inactive selection gesture.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            granularity: None,
            anchor: 0..0,
        }
    }

    /// Start a gesture at `byte_index`.
    ///
    /// Without `extend`, the unit at `byte_index` replaces the selection and
    /// becomes the drag anchor. With `extend` (for example shift-click), the
    /// existing selection anchor (or cursor) stays fixed and the selection
    /// extends to the unit at `byte_index`.
    pub fn begin(
        &mut self,
        buffer: &mut TextEditBuffer,
        byte_index: usize,
        granularity: SelectionGranularity,
        extend: bool,
    ) {
        self.granularity = Some(granularity);
        if extend {
            let fixed = buffer.selection_anchor.unwrap_or(buffer.cursor);
            self.anchor = fixed..fixed;
            self.drag(buffer, byte_index);
            return;
        }
        match granularity {
            SelectionGranularity::Character => {
                buffer.move_cursor(TextMotion::Absolute(byte_index));
                self.anchor = buffer.cursor..buffer.cursor;
            }
            SelectionGranularity::Word => {
                if let Some(range) = word_range_at(buffer.text(), byte_index) {
                    select(buffer, range.start, range.end);
                    self.anchor = range;
                } else {
                    buffer.move_cursor(TextMotion::Absolute(byte_index));
                    self.anchor = buffer.cursor..buffer.cursor;
                }
            }
            SelectionGranularity::Line => {
                let range = line_range_at(buffer.text(), byte_index);
                select(buffer, range.start, range.end);
                self.anchor = range;
            }
            SelectionGranularity::All => {
                buffer.select_all();
                self.anchor = 0..buffer.text().len();
            }
        }
    }

    /// Continue an active gesture to `byte_index`. Does nothing when inactive.
    ///
    /// Dragging forward selects from the anchor unit's start to the end of the
    /// target unit; dragging backward selects from the target unit's start to
    /// the anchor unit's end.
    pub fn drag(&mut self, buffer: &mut TextEditBuffer, byte_index: usize) {
        let Some(granularity) = self.granularity else {
            return;
        };
        let text = buffer.text();
        let target = match granularity {
            SelectionGranularity::Character => {
                let index = byte_index.min(text.len());
                index..index
            }
            SelectionGranularity::Word => word_range_at(text, byte_index).unwrap_or_else(|| {
                let index = byte_index.min(text.len());
                index..index
            }),
            SelectionGranularity::Line => line_range_at(text, byte_index),
            SelectionGranularity::All => {
                buffer.select_all();
                return;
            }
        };
        let len = text.len();
        let anchor_start = self.anchor.start.min(len);
        let anchor_end = self.anchor.end.min(len).max(anchor_start);
        if target.start < anchor_start {
            select(buffer, anchor_end, target.start);
        } else {
            select(buffer, anchor_start, target.end.max(anchor_end));
        }
    }

    /// Finish the gesture; later drags are ignored until the next begin.
    pub const fn end(&mut self) {
        self.granularity = None;
    }

    /// Whether a gesture is in progress.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.granularity.is_some()
    }

    /// Granularity of the gesture in progress.
    #[must_use]
    pub const fn granularity(&self) -> Option<SelectionGranularity> {
        self.granularity
    }

    /// Byte range of the anchor unit while a gesture is in progress.
    #[must_use]
    pub fn anchor(&self) -> Option<Range<usize>> {
        self.granularity.map(|_| self.anchor.clone())
    }

    /// Resume a gesture captured with [`granularity`](Self::granularity) and
    /// [`anchor`](Self::anchor), for example after restoring host state.
    #[must_use]
    pub const fn resume(granularity: SelectionGranularity, anchor: Range<usize>) -> Self {
        Self {
            granularity: Some(granularity),
            anchor,
        }
    }
}

/// Place the selection anchor at `anchor` and the cursor at `cursor`.
fn select(buffer: &mut TextEditBuffer, anchor: usize, cursor: usize) {
    buffer.move_cursor(TextMotion::Absolute(anchor));
    buffer.move_cursor_with_selection(TextMotion::Absolute(cursor), SelectionMode::Extend);
}

/// Byte range of the word at `byte_index`.
///
/// Words are runs of graphemes containing an alphanumeric character or `_`,
/// grown outward from the hit grapheme; a hit on other non-whitespace (such as
/// punctuation) also absorbs adjacent word graphemes. A whitespace hit
/// resolves to the next word, or to the trailing word when only whitespace
/// follows. Returns `None` for empty or all-whitespace text.
#[must_use]
pub fn word_range_at(text: &str, byte_index: usize) -> Option<Range<usize>> {
    let index = byte_index.min(text.len());
    let ranges = text
        .grapheme_indices(true)
        .map(|(start, grapheme)| (start, start.saturating_add(grapheme.len()), grapheme))
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        return None;
    }
    let is_space = |grapheme: &str| grapheme.chars().all(char::is_whitespace);
    let mut position = ranges
        .iter()
        .position(|(start, end, _)| index >= *start && index < *end)
        .unwrap_or(ranges.len() - 1);
    if is_space(ranges[position].2) {
        position = ranges
            .iter()
            .enumerate()
            .skip(position)
            .find(|(_, (_, _, grapheme))| !is_space(grapheme))
            .map_or(position, |(index, _)| index);
    }
    if is_space(ranges[position].2) {
        return None;
    }
    let mut start_position = position;
    while start_position > 0 && is_word_grapheme(ranges[start_position - 1].2) {
        start_position -= 1;
    }
    let mut end_position = position;
    while end_position + 1 < ranges.len() && is_word_grapheme(ranges[end_position + 1].2) {
        end_position += 1;
    }
    Some(ranges[start_position].0..ranges[end_position].1)
}

/// Byte range of the hard line containing `byte_index`, including its
/// trailing `'\n'` when present. An index at a `'\n'` belongs to the line that
/// newline terminates.
#[must_use]
pub fn line_range_at(text: &str, byte_index: usize) -> Range<usize> {
    let mut index = byte_index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    let start = text[..index].rfind('\n').map_or(0, |newline| newline + 1);
    let end = text[index..]
        .find('\n')
        .map_or(text.len(), |newline| index + newline + 1);
    start..end
}

fn is_word_grapheme(grapheme: &str) -> bool {
    grapheme.chars().any(|ch| ch.is_alphanumeric() || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected(buffer: &TextEditBuffer) -> Option<&str> {
        buffer
            .selection()
            .map(|selection| &buffer.text()[selection.start..selection.end])
    }

    fn press(
        text: &str,
        byte_index: usize,
        granularity: SelectionGranularity,
    ) -> (TextEditBuffer, PointerSelection) {
        let mut buffer = TextEditBuffer::from_text(text);
        let mut pointer = PointerSelection::default();
        pointer.begin(&mut buffer, byte_index, granularity, false);
        (buffer, pointer)
    }

    #[test]
    fn click_tracker_counts_within_window_and_distance() {
        let policy = ClickPolicy::default();
        let mut tracker = ClickTracker::new();
        let start = Instant::now();
        assert_eq!(tracker.register(4, 1, start, &policy), 1);
        assert_eq!(
            tracker.register(6, 2, start + Duration::from_millis(100), &policy),
            2
        );
        assert_eq!(
            tracker.register(5, 1, start + Duration::from_millis(200), &policy),
            3
        );
        assert_eq!(
            tracker.register(5, 1, start + Duration::from_millis(300), &policy),
            3,
            "count saturates at max_count"
        );
        assert_eq!(tracker.last_click().map(|click| click.count), Some(3));
    }

    #[test]
    fn click_tracker_resets_when_slow_far_or_reset() {
        let policy = ClickPolicy::default();
        let mut tracker = ClickTracker::new();
        let start = Instant::now();
        assert_eq!(tracker.register(0, 0, start, &policy), 1);
        let late = start + policy.window + Duration::from_millis(1);
        assert_eq!(tracker.register(0, 0, late, &policy), 1);
        assert_eq!(tracker.register(3, 0, late, &policy), 1, "too far in x");
        assert_eq!(tracker.register(3, 3, late, &policy), 1, "too far in y");
        assert_eq!(tracker.register(3, 3, late, &policy), 2);
        tracker.reset();
        assert_eq!(tracker.last_click(), None);
        assert_eq!(tracker.register(3, 3, late, &policy), 1);
    }

    #[test]
    fn click_tracker_honors_max_count_bounds() {
        let start = Instant::now();
        let four = ClickPolicy::with_max_count(4);
        let mut tracker = ClickTracker::new();
        let counts = (0..6)
            .map(|_| tracker.register(0, 0, start, &four))
            .collect::<Vec<_>>();
        assert_eq!(counts, [1, 2, 3, 4, 4, 4]);

        let zero = ClickPolicy::with_max_count(0);
        let mut tracker = ClickTracker::new();
        assert_eq!(tracker.register(0, 0, start, &zero), 1);
        assert_eq!(tracker.register(0, 0, start, &zero), 1);

        let huge = ClickPolicy::with_max_count(u8::MAX);
        let mut tracker = ClickTracker::new();
        let last = (0..20).map(|_| tracker.register(0, 0, start, &huge)).last();
        assert_eq!(last, Some(ClickPolicy::MAX_COUNT));
    }

    #[test]
    fn click_tracker_restores_last_click() {
        let policy = ClickPolicy::default();
        let start = Instant::now();
        let mut tracker = ClickTracker::new();
        tracker.register(1, 1, start, &policy);
        let mut restored = ClickTracker::from_last_click(tracker.last_click());
        assert_eq!(restored, tracker);
        assert_eq!(restored.register(1, 1, start, &policy), 2);
    }

    #[test]
    fn click_count_maps_through_sequences() {
        use SelectionGranularity::{All, Character, Line, Word};
        let field = FIELD_CLICK_GRANULARITIES;
        let editor = EDITOR_CLICK_GRANULARITIES;
        assert_eq!(granularity_for_click_count(field, 0), Character);
        assert_eq!(granularity_for_click_count(field, 1), Character);
        assert_eq!(granularity_for_click_count(field, 2), Word);
        assert_eq!(granularity_for_click_count(field, 3), All);
        assert_eq!(granularity_for_click_count(field, 9), All);
        assert_eq!(granularity_for_click_count(editor, 3), Line);
        assert_eq!(granularity_for_click_count(editor, 4), All);
        assert_eq!(granularity_for_click_count(&[], 2), Character);
    }

    #[test]
    fn word_range_handles_unicode_punctuation_and_whitespace() {
        assert_eq!(word_range_at("", 0), None);
        assert_eq!(word_range_at("   ", 1), None);
        assert_eq!(word_range_at("hello world", 1), Some(0..5));
        assert_eq!(
            word_range_at("hello world", 5),
            Some(6..11),
            "space skips forward"
        );
        assert_eq!(word_range_at("hello world", 99), Some(6..11));
        assert_eq!(word_range_at("snake_case.x", 2), Some(0..10));
        assert_eq!(
            word_range_at("a.b", 1),
            Some(0..3),
            "punctuation joins adjacent words"
        );
        assert_eq!(word_range_at("a .. b", 2), Some(2..3));
        let text = "caf\u{e9} na\u{ef}ve e\u{301}x";
        assert_eq!(word_range_at(text, 0), Some(0..5));
        let combining = text.find("e\u{301}").unwrap();
        assert_eq!(word_range_at(text, combining), Some(combining..text.len()));
        assert_eq!(word_range_at("\u{65e5}\u{672c}", 3), Some(0..6));
    }

    #[test]
    fn line_range_includes_newline_and_handles_edges() {
        let text = "one\ntwo\nthree";
        assert_eq!(line_range_at(text, 0), 0..4);
        assert_eq!(line_range_at(text, 3), 0..4, "newline belongs to its line");
        assert_eq!(line_range_at(text, 4), 4..8);
        assert_eq!(line_range_at(text, 10), 8..13, "last line without newline");
        assert_eq!(line_range_at(text, 99), 8..13);
        assert_eq!(line_range_at("", 0), 0..0);
        assert_eq!(line_range_at("a\n", 2), 2..2, "empty trailing line");
        assert_eq!(line_range_at("\n\n", 1), 1..2);
        let unicode = "\u{e9}t\u{e9}\n\u{65e5}";
        assert_eq!(
            line_range_at(unicode, 1),
            0..6,
            "non-boundary index snaps back"
        );
        assert_eq!(line_range_at(unicode, 7), 6..9);
    }

    #[test]
    fn character_gesture_drags_both_directions() {
        let (mut buffer, mut pointer) = press("hello world", 5, SelectionGranularity::Character);
        assert_eq!(selected(&buffer), None);
        assert_eq!(buffer.cursor_byte_index(), 5);
        pointer.drag(&mut buffer, 8);
        assert_eq!(selected(&buffer), Some(" wo"));
        pointer.drag(&mut buffer, 2);
        assert_eq!(selected(&buffer), Some("llo"));
        assert_eq!(buffer.cursor_byte_index(), 2);
        pointer.drag(&mut buffer, 5);
        assert_eq!(selected(&buffer), None);
    }

    #[test]
    fn word_gesture_keeps_anchor_word_in_both_directions() {
        let text = "alpha beta gamma";
        let (mut buffer, mut pointer) = press(text, 7, SelectionGranularity::Word);
        assert_eq!(selected(&buffer), Some("beta"));
        assert_eq!(pointer.anchor(), Some(6..10));

        pointer.drag(&mut buffer, 13);
        assert_eq!(selected(&buffer), Some("beta gamma"));
        assert_eq!(buffer.cursor_byte_index(), text.len());

        pointer.drag(&mut buffer, 2);
        assert_eq!(selected(&buffer), Some("alpha beta"));
        assert_eq!(buffer.cursor_byte_index(), 0);

        pointer.drag(&mut buffer, 8);
        assert_eq!(
            selected(&buffer),
            Some("beta"),
            "inside anchor keeps anchor"
        );
    }

    #[test]
    fn line_gesture_keeps_anchor_line_in_both_directions() {
        let text = "one\ntwo\nthree";
        let (mut buffer, mut pointer) = press(text, 5, SelectionGranularity::Line);
        assert_eq!(selected(&buffer), Some("two\n"));
        pointer.drag(&mut buffer, 10);
        assert_eq!(selected(&buffer), Some("two\nthree"));
        pointer.drag(&mut buffer, 1);
        assert_eq!(selected(&buffer), Some("one\ntwo\n"));
        assert_eq!(buffer.cursor_byte_index(), 0);
    }

    #[test]
    fn all_gesture_selects_everything() {
        let (mut buffer, mut pointer) = press("a\nb", 1, SelectionGranularity::All);
        assert_eq!(selected(&buffer), Some("a\nb"));
        pointer.drag(&mut buffer, 0);
        assert_eq!(selected(&buffer), Some("a\nb"));
    }

    #[test]
    fn word_gesture_on_whitespace_only_text_places_cursor() {
        let (mut buffer, mut pointer) = press("   ", 1, SelectionGranularity::Word);
        assert_eq!(selected(&buffer), None);
        assert_eq!(buffer.cursor_byte_index(), 1);
        pointer.drag(&mut buffer, 3);
        assert_eq!(selected(&buffer), Some("  "));
    }

    #[test]
    fn end_stops_drag_and_reports_state() {
        let (mut buffer, mut pointer) = press("hello world", 1, SelectionGranularity::Word);
        assert!(pointer.is_active());
        assert_eq!(pointer.granularity(), Some(SelectionGranularity::Word));
        pointer.end();
        assert!(!pointer.is_active());
        assert_eq!(pointer.granularity(), None);
        assert_eq!(pointer.anchor(), None);
        pointer.drag(&mut buffer, 9);
        assert_eq!(selected(&buffer), Some("hello"));
    }

    #[test]
    fn extend_keeps_existing_anchor_at_granularity() {
        let text = "alpha beta gamma";
        let mut buffer = TextEditBuffer::from_text(text);
        buffer.move_cursor(TextMotion::Absolute(2));
        let mut pointer = PointerSelection::default();
        pointer.begin(&mut buffer, 8, SelectionGranularity::Character, true);
        assert_eq!(selected(&buffer), Some("pha be"));

        pointer.begin(&mut buffer, 12, SelectionGranularity::Word, true);
        assert_eq!(
            selected(&buffer),
            Some("pha beta gamma"),
            "anchor stays at 2"
        );
        pointer.drag(&mut buffer, 0);
        assert_eq!(selected(&buffer), Some("al"));
        assert_eq!(buffer.cursor_byte_index(), 0);

        let mut buffer = TextEditBuffer::from_text("one\ntwo\nthree");
        buffer.move_cursor(TextMotion::Absolute(9));
        pointer.begin(&mut buffer, 1, SelectionGranularity::Line, true);
        assert_eq!(selected(&buffer), Some("one\ntwo\nt"));
    }

    #[test]
    fn drag_clamps_stale_anchor_after_text_shrinks() {
        let mut buffer = TextEditBuffer::from_text("abc");
        let mut pointer = PointerSelection::resume(SelectionGranularity::Word, 10..20);
        pointer.drag(&mut buffer, 0);
        assert_eq!(selected(&buffer), Some("abc"));
    }
}
