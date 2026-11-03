//! Transient, bounded editor transactions and undo/redo.

use std::collections::VecDeque;
use std::fmt;
use std::ops::Range;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::TextEditBuffer;
use unicode_segmentation::UnicodeSegmentation;

/// Maximum retained undo and redo transactions combined.
pub const MAX_HISTORY_ENTRIES: usize = 128;
/// Maximum retained UTF-8 snapshot bytes across undo and redo combined.
pub const MAX_HISTORY_BYTES: usize = 1_048_576;

/// Exact editor state used for stale validation and transactional replacement.
///
/// Positions are UTF-8 byte offsets. Selection direction is preserved by its
/// anchor and cursor rather than a sorted range. This is not a durable revision:
/// applications requiring generation identity must validate that separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEditState {
    /// Complete original text.
    pub text: String,
    /// Insertion position.
    pub cursor: usize,
    /// Selection anchor, including a collapsed selection when present.
    pub selection_anchor: Option<usize>,
}

/// Payload-free atomic replacement failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEditTransactionError {
    /// Expected text, cursor or selection no longer matches the editor.
    Stale,
    /// Range is not an ordered, in-bounds UTF-8 byte range.
    InvalidRange,
    /// Resulting cursor or selection is not an in-bounds grapheme boundary.
    InvalidPosition,
}

impl fmt::Display for TextEditTransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stale => "stale editor transaction",
            Self::InvalidRange => "invalid editor byte range",
            Self::InvalidPosition => "invalid editor position",
        })
    }
}
impl std::error::Error for TextEditTransactionError {}

/// How consecutive edits combine into undo steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UndoGrouping {
    /// Every edit is its own undo step.
    Keystroke,
    /// Consecutive typing groups into words: starting a new word starts a new
    /// step. Otherwise follows [`Self::Smart`].
    Word,
    /// Consecutive edits of one kind (typing, deleting backward, deleting
    /// forward) group into one step. A step ends at a pause longer than
    /// [`UndoPolicy::pause`], a switch between typing and deleting, a cursor
    /// jump, or a newline (which belongs to the step it ends). Pastes and
    /// other edits are always their own steps, and a run lasting longer than
    /// [`UndoPolicy::max_run`] splits at the next word.
    Smart,
}

/// Undo grouping policy for a [`TextEditBuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UndoPolicy {
    /// How edits combine.
    pub grouping: UndoGrouping,
    /// A longer pause between edits starts a new step.
    pub pause: Duration,
    /// Under [`UndoGrouping::Smart`], an uninterrupted run longer than this
    /// splits at the next word, so long bursts stay undoable in pieces.
    pub max_run: Duration,
}

impl UndoPolicy {
    /// Smart grouping with a half-second pause and three-second runs.
    pub const DEFAULT: Self = Self {
        grouping: UndoGrouping::Smart,
        pause: Duration::from_millis(500),
        max_run: Duration::from_secs(3),
    };

    /// One undo step per edit.
    pub const KEYSTROKE: Self = Self {
        grouping: UndoGrouping::Keystroke,
        ..Self::DEFAULT
    };
}

impl Default for UndoPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What an edit did, for undo grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Typed text (characters or a newline).
    Insert,
    /// Deleted the grapheme before the cursor.
    DeleteBackward,
    /// Deleted the grapheme at the cursor.
    DeleteForward,
    /// Pastes, clears, selection deletes, word/line deletes, transactions:
    /// always their own step.
    Other,
}

/// The step currently accepting merges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenGroup {
    kind: EditKind,
    started: Instant,
    last: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RetainedState {
    state: TextEditState,
    desired_visual_col: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Transaction {
    before: RetainedState,
    after: RetainedState,
}
impl Transaction {
    const fn bytes(&self) -> usize {
        self.before
            .state
            .text
            .len()
            .saturating_add(self.after.state.text.len())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditHistory {
    undo: VecDeque<Transaction>,
    redo: VecDeque<Transaction>,
    policy: UndoPolicy,
    open: Option<OpenGroup>,
    input_time: Option<Instant>,
}

impl Default for EditHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl EditHistory {
    pub const fn new() -> Self {
        Self::with_policy(UndoPolicy::DEFAULT)
    }

    const fn with_policy(policy: UndoPolicy) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            policy,
            open: None,
            input_time: None,
        }
    }

    /// Whether an edit of `kind` at `now`, turning `before` into `after`,
    /// continues the open step.
    fn continues(
        &self,
        kind: EditKind,
        before: &RetainedState,
        after: &RetainedState,
        now: Instant,
    ) -> bool {
        let policy = self.policy;
        let Some(open) = self.open else {
            return false;
        };
        if policy.grouping == UndoGrouping::Keystroke
            || kind == EditKind::Other
            || kind != open.kind
            || !self.redo.is_empty()
            || now.saturating_duration_since(open.last) > policy.pause
            // The edit must continue exactly where the step ended: no cursor
            // movement, selection change, or other edit in between.
            || self.undo.back().is_none_or(|last| last.after.state != before.state)
        {
            return false;
        }
        if kind != EditKind::Insert {
            return now.saturating_duration_since(open.started) <= policy.max_run;
        }
        let starts_word = starts_word(&before.state, &after.state);
        match policy.grouping {
            UndoGrouping::Word => !starts_word,
            UndoGrouping::Smart => {
                !starts_word || now.saturating_duration_since(open.started) <= policy.max_run
            }
            UndoGrouping::Keystroke => false,
        }
    }

    fn prepare(&mut self, bytes: usize) {
        self.redo.clear();
        if bytes > MAX_HISTORY_BYTES {
            // An unretained edit breaks the undo chain; never restore older text
            // through a gap whose before/after state was not retained.
            self.undo.clear();
            return;
        }
        while self.undo.len() >= MAX_HISTORY_ENTRIES
            || self.undo.iter().map(Transaction::bytes).sum::<usize>() > MAX_HISTORY_BYTES - bytes
        {
            self.undo.pop_front();
        }
    }
}

impl TextEditBuffer {
    /// Capture the exact text, cursor and selection for stale-checked replacement.
    #[must_use]
    pub fn edit_state(&self) -> TextEditState {
        TextEditState {
            text: self.text.clone(),
            cursor: self.cursor,
            selection_anchor: self.selection_anchor,
        }
    }

    fn retained_state(&self) -> RetainedState {
        RetainedState {
            state: self.edit_state(),
            desired_visual_col: self.desired_visual_col,
        }
    }

    fn restore_state(&mut self, retained: &RetainedState) {
        self.text.clone_from(&retained.state.text);
        self.cursor = retained.state.cursor;
        self.selection_anchor = retained.state.selection_anchor;
        self.desired_visual_col = retained.desired_visual_col;
    }

    pub(crate) fn record_edit<T>(
        &mut self,
        kind: EditKind,
        edit: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let now = self.history.input_time.take().unwrap_or_else(Instant::now);
        let before = self.retained_state();
        // Replacing a selection starts a step that following typing may join;
        // deleting a selection is a step of its own.
        let replaced = before
            .state
            .selection_anchor
            .is_some_and(|anchor| anchor != before.state.cursor);
        let kind = match kind {
            EditKind::DeleteBackward | EditKind::DeleteForward if replaced => EditKind::Other,
            kind => kind,
        };
        let result = edit(self);
        if self.text == before.state.text {
            self.restore_state(&before);
        } else if before.state.text.len().saturating_add(self.text.len()) > MAX_HISTORY_BYTES {
            // Keep the valid content but break the chain before cloning an
            // oversized after snapshot. Never undo across an unrecorded edit.
            self.clear_history();
        } else {
            let after = self.retained_state();
            let merge = !replaced && self.history.continues(kind, &before, &after, now);
            let before = if merge {
                // Extend the open step: it keeps its original before-state.
                self.history
                    .undo
                    .pop_back()
                    .map_or(before, |step| step.before)
            } else {
                before
            };
            // Prune both count and byte budgets before retaining the step.
            self.history
                .prepare(before.state.text.len() + after.state.text.len());
            let ends_line = kind == EditKind::Insert
                && after.state.text.len() > before.state.text.len()
                && inserted(&before.state, &after.state).contains('\n');
            self.history.open = match (kind, self.history.open) {
                (EditKind::Other, _) => None,
                // A newline belongs to the step it ends.
                _ if ends_line => None,
                (_, Some(open)) if merge => Some(OpenGroup { last: now, ..open }),
                _ => Some(OpenGroup {
                    kind,
                    started: now,
                    last: now,
                }),
            };
            self.history.undo.push_back(Transaction { before, after });
        }
        result
    }

    /// Undo grouping policy.
    #[must_use]
    pub const fn undo_policy(&self) -> UndoPolicy {
        self.history.policy
    }

    /// Set how consecutive edits combine into undo steps.
    pub const fn set_undo_policy(&mut self, policy: UndoPolicy) {
        self.history.policy = policy;
        self.history.open = None;
    }

    /// End the current undo step: the next edit starts a new one (for example
    /// when a command runs or the editor loses focus).
    pub const fn break_undo_group(&mut self) {
        self.history.open = None;
    }

    /// Use `now` as the time of the next edit instead of the clock (for
    /// replaying input or deterministic tests). Applies to one edit.
    pub const fn set_input_time(&mut self, now: Instant) {
        self.history.input_time = Some(now);
    }

    /// Replace one UTF-8 byte range as one validated undoable transaction.
    ///
    /// Compare `expected` to the live editor before preparing the replacement.
    /// Resulting positions must be grapheme boundaries; range endpoints need only
    /// be UTF-8 boundaries. A text-identical replacement returns false and leaves
    /// cursor, selection and undo/redo unchanged. Failed operations change nothing.
    ///
    /// # Errors
    /// Returns `Stale` for mismatched expected state, `InvalidRange` for invalid
    /// UTF-8 ranges, or `InvalidPosition` for invalid resulting positions.
    pub fn replace_byte_range(
        &mut self,
        expected: &TextEditState,
        range: Range<usize>,
        replacement: &str,
        cursor: usize,
        selection_anchor: Option<usize>,
    ) -> Result<bool, TextEditTransactionError> {
        if expected.text != self.text
            || expected.cursor != self.cursor
            || expected.selection_anchor != self.selection_anchor
        {
            return Err(TextEditTransactionError::Stale);
        }
        self.text
            .get(range.clone())
            .ok_or(TextEditTransactionError::InvalidRange)?;
        let mut text = self.text.clone();
        text.replace_range(range, replacement);
        let valid_position = |position| {
            position <= text.len()
                && text.is_char_boundary(position)
                && (position == text.len()
                    || text
                        .grapheme_indices(true)
                        .any(|(start, _)| start == position))
        };
        if !valid_position(cursor) || selection_anchor.is_some_and(|anchor| !valid_position(anchor))
        {
            return Err(TextEditTransactionError::InvalidPosition);
        }
        if text == self.text {
            return Ok(false);
        }
        self.record_edit(EditKind::Other, |editor| {
            editor.text = text;
            editor.cursor = cursor;
            editor.selection_anchor = selection_anchor;
            editor.desired_visual_col = None;
        });
        Ok(true)
    }

    /// Whether a retained edit can be undone.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.history.undo.is_empty()
    }

    /// Whether an undone edit can be redone.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.history.redo.is_empty()
    }

    /// Restore the last edit's complete before-state, preserving selection direction.
    /// Cursor-only movements do not create entries; undo restores the edit boundary.
    pub fn undo(&mut self) -> bool {
        let Some(transaction) = self.history.undo.pop_back() else {
            return false;
        };
        self.restore_state(&transaction.before);
        self.history.redo.push_back(transaction);
        self.history.open = None;
        true
    }

    /// Restore the most recently undone edit's complete after-state.
    pub fn redo(&mut self) -> bool {
        let Some(transaction) = self.history.redo.pop_back() else {
            return false;
        };
        self.restore_state(&transaction.after);
        self.history.undo.push_back(transaction);
        self.history.open = None;
        true
    }

    /// Dispose transient undo/redo, for document lifecycle changes or privacy policy.
    /// Serialization never retains history; callers should not serialize undo text.
    /// Successful edits exceeding the history byte cap keep their content and clear
    /// both stacks, preventing undo across an unrecorded edit.
    /// The undo grouping policy is kept.
    pub fn clear_history(&mut self) {
        self.history = EditHistory::with_policy(self.history.policy);
    }
}

/// Text inserted by an edit that only inserted at `before.cursor`.
fn inserted<'a>(before: &TextEditState, after: &'a TextEditState) -> &'a str {
    let added = after.text.len().saturating_sub(before.text.len());
    after
        .text
        .get(before.cursor..before.cursor + added)
        .unwrap_or_default()
}

/// Whether an insertion starts a new word: its first character is a word
/// character typed after a non-word character (or at the start).
fn starts_word(before: &TextEditState, after: &TextEditState) -> bool {
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
    let first = inserted(before, after).chars().next();
    let previous = before.text[..before.cursor].chars().next_back();
    first.is_some_and(is_word) && !previous.is_some_and(is_word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SelectionMode, TextMotion};

    #[test]
    fn atomic_unicode_replacement_restores_selection_direction() {
        let mut editor = TextEditBuffer::from_text("é🦀x");
        editor.move_cursor(TextMotion::Absolute(6));
        editor.move_cursor_with_selection(TextMotion::Absolute(2), SelectionMode::Extend);
        let before = editor.edit_state();
        assert!(
            editor
                .replace_byte_range(&before, 2..6, "中", 5, Some(2))
                .unwrap()
        );
        let after = editor.edit_state();
        assert_eq!(after.text, "é中x");
        assert!(editor.undo());
        assert_eq!(editor.edit_state(), before);
        assert!(editor.redo());
        assert_eq!(editor.edit_state(), after);
    }

    #[test]
    fn invalid_stale_and_noop_leave_history_and_state_untouched() {
        let mut editor = TextEditBuffer::from_text("é");
        editor.insert_str("x");
        assert!(editor.undo());
        let before = editor.clone();
        let expected = editor.edit_state();
        assert_eq!(
            editor.replace_byte_range(&expected, 1..2, "z", 2, None),
            Err(TextEditTransactionError::InvalidRange)
        );
        assert_eq!(editor, before);
        assert_eq!(
            editor.replace_byte_range(&expected, 0..2, "e\u{301}", 1, None),
            Err(TextEditTransactionError::InvalidPosition)
        );
        assert_eq!(editor, before);
        let mut stale = expected.clone();
        stale.cursor = 0;
        assert_eq!(
            editor.replace_byte_range(&stale, 0..2, "z", 1, None),
            Err(TextEditTransactionError::Stale)
        );
        assert_eq!(editor, before);
        assert!(
            !editor
                .replace_byte_range(&expected, 0..2, "é", 0, None)
                .unwrap()
        );
        assert_eq!(editor, before);
        editor.insert_str("");
        assert_eq!(editor, before);
        assert!(editor.redo());
        assert_eq!(editor.text(), "éx");
    }

    #[test]
    fn selection_paste_and_cut_are_single_transactions() {
        let mut editor = TextEditBuffer::from_text("abc");
        editor.select_all();
        let before = editor.edit_state();
        editor.paste("x\r\ny");
        assert_eq!(editor.text(), "x\ny");
        assert!(editor.undo());
        assert_eq!(editor.edit_state(), before);
        assert!(!editor.can_undo());
        editor.cut_selection();
        assert_eq!(editor.text(), "");
        assert!(!editor.can_redo());
        assert!(editor.undo());
        assert_eq!(editor.edit_state(), before);
    }

    #[test]
    fn bounded_history_and_oversized_edits_break_chain_safely() {
        let mut editor = TextEditBuffer::new();
        editor.set_undo_policy(UndoPolicy::KEYSTROKE);
        for _ in 0..MAX_HISTORY_ENTRIES + 10 {
            editor.insert_char('x');
        }
        let mut count = 0;
        while editor.undo() {
            count += 1;
        }
        assert_eq!(count, MAX_HISTORY_ENTRIES);
        editor.insert_str(&"x".repeat(MAX_HISTORY_BYTES + 1));
        assert!(!editor.can_undo());
        assert!(!editor.can_redo());
        editor.clear_history();
    }

    /// Type `text` one character at a time, `gap` apart, starting at `start`.
    fn type_at(editor: &mut TextEditBuffer, text: &str, start: Instant, gap: Duration) -> Instant {
        let mut now = start;
        for ch in text.chars() {
            editor.set_input_time(now);
            editor.insert_char(ch);
            now += gap;
        }
        now
    }

    fn undo_steps(editor: &mut TextEditBuffer) -> Vec<String> {
        let mut steps = Vec::new();
        while editor.undo() {
            steps.push(editor.text().to_string());
        }
        steps
    }

    const FAST: Duration = Duration::from_millis(80);

    #[test]
    fn smart_grouping_merges_bursts_and_splits_at_pauses() {
        let mut editor = TextEditBuffer::new();
        assert_eq!(editor.undo_policy(), UndoPolicy::DEFAULT);
        let start = Instant::now();
        let now = type_at(&mut editor, "select 1", start, FAST);
        type_at(&mut editor, " from t", now + Duration::from_secs(1), FAST);
        assert_eq!(undo_steps(&mut editor), vec!["select 1", ""]);
        // Redo replays whole steps.
        assert!(editor.redo());
        assert_eq!(editor.text(), "select 1");
        assert!(editor.redo());
        assert_eq!(editor.text(), "select 1 from t");
    }

    #[test]
    fn smart_grouping_breaks_on_kind_changes_jumps_newlines_and_pastes() {
        let mut editor = TextEditBuffer::new();
        let mut now = type_at(&mut editor, "abc", Instant::now(), FAST);
        // Switching to deleting starts a step; backspaces group.
        for _ in 0..2 {
            editor.set_input_time(now);
            editor.delete_backward();
            now += FAST;
        }
        // A newline ends its step.
        now = type_at(&mut editor, "x\n", now, FAST);
        now = type_at(&mut editor, "y", now, FAST);
        // A cursor jump starts a step.
        editor.move_cursor(TextMotion::Start);
        now = type_at(&mut editor, "z", now, FAST);
        editor.move_cursor(TextMotion::End);
        editor.set_input_time(now);
        editor.paste("PP");
        assert_eq!(
            undo_steps(&mut editor),
            vec!["zax\ny", "ax\ny", "ax\n", "a", "abc", ""]
        );
    }

    #[test]
    fn long_smart_runs_split_at_words_and_word_mode_splits_every_word() {
        let mut editor = TextEditBuffer::new();
        // 30 characters 200ms apart: six seconds of uninterrupted typing.
        type_at(
            &mut editor,
            "alpha beta gamma delta epsilon",
            Instant::now(),
            Duration::from_millis(200),
        );
        let steps = undo_steps(&mut editor);
        assert!(steps.len() > 1 && steps.len() < 5, "{steps:?}");
        assert!(
            steps
                .iter()
                .all(|text| text.is_empty() || text.ends_with(' ')),
            "runs split where a word starts: {steps:?}"
        );

        let mut editor = TextEditBuffer::new();
        editor.set_undo_policy(UndoPolicy {
            grouping: UndoGrouping::Word,
            ..UndoPolicy::DEFAULT
        });
        type_at(&mut editor, "select a, b", Instant::now(), FAST);
        assert_eq!(undo_steps(&mut editor), vec!["select a, ", "select ", ""]);
    }

    #[test]
    fn keystroke_grouping_and_explicit_breaks() {
        let mut editor = TextEditBuffer::new();
        editor.set_undo_policy(UndoPolicy::KEYSTROKE);
        type_at(&mut editor, "abc", Instant::now(), FAST);
        assert_eq!(undo_steps(&mut editor), vec!["ab", "a", ""]);

        let mut editor = TextEditBuffer::new();
        let now = type_at(&mut editor, "ab", Instant::now(), FAST);
        editor.break_undo_group();
        type_at(&mut editor, "cd", now, FAST);
        assert_eq!(undo_steps(&mut editor), vec!["ab", ""]);

        // Typing over a selection starts a step that following typing joins.
        let mut editor = TextEditBuffer::from_text("old");
        editor.select_all();
        type_at(&mut editor, "new", Instant::now(), FAST);
        assert_eq!(undo_steps(&mut editor), vec!["old"]);
        // The policy survives clearing history.
        editor.set_undo_policy(UndoPolicy::KEYSTROKE);
        editor.clear_history();
        assert_eq!(editor.undo_policy(), UndoPolicy::KEYSTROKE);
    }

    #[test]
    fn ordinary_mutators_share_one_history_and_preserve_redo_on_noops() {
        use crate::{TextDelete, TextEditCommand};
        let mut editor = TextEditBuffer::from_text("abc");
        editor.delete_backward();
        assert_eq!(editor.text(), "ab");
        assert!(editor.undo());
        editor.move_cursor(TextMotion::Start);
        editor.delete_backward();
        assert!(editor.can_redo());
        editor.redo();
        editor.move_cursor(TextMotion::Start);
        editor.delete_forward();
        assert_eq!(editor.text(), "b");
        editor.undo();
        assert_eq!(editor.text(), "ab");
        editor.apply_command(TextEditCommand::Delete(TextDelete::ToEnd));
        assert_eq!(editor.text(), "");
        editor.undo();
        assert_eq!(editor.text(), "ab");
        editor.insert_newline();
        editor.undo();
        assert_eq!(editor.text(), "ab");
        editor.clear();
        editor.undo();
        assert_eq!(editor.text(), "ab");
        editor.clear_history();
        assert!(!editor.can_undo());
        assert!(!editor.can_redo());
    }

    #[test]
    fn byte_budget_evicts_before_count_budget() {
        let mut editor = TextEditBuffer::from_text("x".repeat(100_000));
        for _ in 0..10 {
            editor.insert_char('y');
        }
        assert!(editor.history.undo.len() < MAX_HISTORY_ENTRIES);
        assert!(
            editor
                .history
                .undo
                .iter()
                .map(Transaction::bytes)
                .sum::<usize>()
                <= MAX_HISTORY_BYTES
        );
        for _ in 0..3 {
            editor.undo();
        }
        assert!(
            editor
                .history
                .undo
                .iter()
                .chain(&editor.history.redo)
                .map(Transaction::bytes)
                .sum::<usize>()
                <= MAX_HISTORY_BYTES
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn history_is_transient_across_serde() {
        let mut editor = TextEditBuffer::from_text("before");
        editor.insert_str("after");
        let value = serde_json::to_value(&editor).unwrap();
        assert!(value.get("history").is_none());
        let restored: TextEditBuffer = serde_json::from_value(value).unwrap();
        assert_eq!(restored.edit_state(), editor.edit_state());
        assert!(!restored.can_undo());
    }
}
