//! Transient, bounded editor transactions and undo/redo.

use std::collections::VecDeque;
use std::fmt;
use std::ops::Range;

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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EditHistory {
    undo: VecDeque<Transaction>,
    redo: VecDeque<Transaction>,
}
impl EditHistory {
    pub const fn new() -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
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

    pub(crate) fn record_edit<T>(&mut self, edit: impl FnOnce(&mut Self) -> T) -> T {
        let before = self.retained_state();
        let result = edit(self);
        if self.text == before.state.text {
            self.restore_state(&before);
        } else if before.state.text.len().saturating_add(self.text.len()) > MAX_HISTORY_BYTES {
            // Keep the valid content but break the chain before cloning an
            // oversized after snapshot. Never undo across an unrecorded edit.
            self.clear_history();
        } else {
            // Prune both count and byte budgets before allocating the after-state.
            self.history
                .prepare(before.state.text.len() + self.text.len());
            let after = self.retained_state();
            self.history.undo.push_back(Transaction { before, after });
        }
        result
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
        self.record_edit(|editor| {
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
        true
    }

    /// Restore the most recently undone edit's complete after-state.
    pub fn redo(&mut self) -> bool {
        let Some(transaction) = self.history.redo.pop_back() else {
            return false;
        };
        self.restore_state(&transaction.after);
        self.history.undo.push_back(transaction);
        true
    }

    /// Dispose transient undo/redo, for document lifecycle changes or privacy policy.
    /// Serialization never retains history; callers should not serialize undo text.
    /// Successful edits exceeding the history byte cap keep their content and clear
    /// both stacks, preventing undo across an unrecorded edit.
    pub fn clear_history(&mut self) {
        self.history = EditHistory::new();
    }
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
