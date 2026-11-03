//! Named, bindable text-editing actions.

use crate::{SelectionMode, TextDelete, TextEditBuffer, TextMotion};

/// A named, bindable text-editing action.
///
/// Actions cover everything an editor binding can do except inserting typed
/// characters, which stays implicit. Each action has a stable kebab-case
/// [`id`](Self::id) suitable for configuration files and a human
/// [`label`](Self::label) for help screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum TextEditAction {
    /// Move one grapheme left.
    MoveLeft,
    /// Move one grapheme right.
    MoveRight,
    /// Move to the previous word boundary.
    MoveWordLeft,
    /// Move to the next word boundary.
    MoveWordRight,
    /// Move to the start of the current hard line.
    MoveLineStart,
    /// Move to the end of the current hard line.
    MoveLineEnd,
    /// Move to the start of the buffer.
    MoveStart,
    /// Move to the end of the buffer.
    MoveEnd,
    /// Move one visual line up.
    MoveUp,
    /// Move one visual line down.
    MoveDown,
    /// Extend the selection one grapheme left.
    SelectLeft,
    /// Extend the selection one grapheme right.
    SelectRight,
    /// Extend the selection to the previous word boundary.
    SelectWordLeft,
    /// Extend the selection to the next word boundary.
    SelectWordRight,
    /// Extend the selection to the start of the current hard line.
    SelectLineStart,
    /// Extend the selection to the end of the current hard line.
    SelectLineEnd,
    /// Extend the selection to the start of the buffer.
    SelectStart,
    /// Extend the selection to the end of the buffer.
    SelectEnd,
    /// Extend the selection one visual line up.
    SelectUp,
    /// Extend the selection one visual line down.
    SelectDown,
    /// Select the whole buffer.
    SelectAll,
    /// Delete the selection or one grapheme before the cursor.
    DeleteBackward,
    /// Delete the selection or one grapheme at the cursor.
    DeleteForward,
    /// Delete the selection or back to the previous word boundary.
    DeleteWordBackward,
    /// Delete the selection or forward to the next word boundary.
    DeleteWordForward,
    /// Delete the selection or back to the start of the current hard line.
    DeleteToLineStart,
    /// Delete the selection or forward to the end of the current hard line.
    DeleteToLineEnd,
    /// Delete the selection or back to the start of the buffer.
    DeleteToStart,
    /// Delete the selection or forward to the end of the buffer.
    DeleteToEnd,
    /// Clear the whole buffer.
    Clear,
    /// Restore the preceding edit state.
    Undo,
    /// Restore the most recently undone edit state.
    Redo,
}

impl TextEditAction {
    /// Every action, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::MoveLeft,
        Self::MoveRight,
        Self::MoveWordLeft,
        Self::MoveWordRight,
        Self::MoveLineStart,
        Self::MoveLineEnd,
        Self::MoveStart,
        Self::MoveEnd,
        Self::MoveUp,
        Self::MoveDown,
        Self::SelectLeft,
        Self::SelectRight,
        Self::SelectWordLeft,
        Self::SelectWordRight,
        Self::SelectLineStart,
        Self::SelectLineEnd,
        Self::SelectStart,
        Self::SelectEnd,
        Self::SelectUp,
        Self::SelectDown,
        Self::SelectAll,
        Self::DeleteBackward,
        Self::DeleteForward,
        Self::DeleteWordBackward,
        Self::DeleteWordForward,
        Self::DeleteToLineStart,
        Self::DeleteToLineEnd,
        Self::DeleteToStart,
        Self::DeleteToEnd,
        Self::Clear,
        Self::Undo,
        Self::Redo,
    ];

    /// Return the stable kebab-case identifier for configuration files.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::MoveLeft => "move-left",
            Self::MoveRight => "move-right",
            Self::MoveWordLeft => "move-word-left",
            Self::MoveWordRight => "move-word-right",
            Self::MoveLineStart => "move-line-start",
            Self::MoveLineEnd => "move-line-end",
            Self::MoveStart => "move-start",
            Self::MoveEnd => "move-end",
            Self::MoveUp => "move-up",
            Self::MoveDown => "move-down",
            Self::SelectLeft => "select-left",
            Self::SelectRight => "select-right",
            Self::SelectWordLeft => "select-word-left",
            Self::SelectWordRight => "select-word-right",
            Self::SelectLineStart => "select-line-start",
            Self::SelectLineEnd => "select-line-end",
            Self::SelectStart => "select-start",
            Self::SelectEnd => "select-end",
            Self::SelectUp => "select-up",
            Self::SelectDown => "select-down",
            Self::SelectAll => "select-all",
            Self::DeleteBackward => "delete-backward",
            Self::DeleteForward => "delete-forward",
            Self::DeleteWordBackward => "delete-word-backward",
            Self::DeleteWordForward => "delete-word-forward",
            Self::DeleteToLineStart => "delete-to-line-start",
            Self::DeleteToLineEnd => "delete-to-line-end",
            Self::DeleteToStart => "delete-to-start",
            Self::DeleteToEnd => "delete-to-end",
            Self::Clear => "clear",
            Self::Undo => "undo",
            Self::Redo => "redo",
        }
    }

    /// Parse a stable identifier produced by [`id`](Self::id).
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|action| action.id() == id)
    }

    /// Return a short human description for help screens.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MoveLeft => "Move left",
            Self::MoveRight => "Move right",
            Self::MoveWordLeft => "Move to previous word",
            Self::MoveWordRight => "Move to next word",
            Self::MoveLineStart => "Move to line start",
            Self::MoveLineEnd => "Move to line end",
            Self::MoveStart => "Move to start of text",
            Self::MoveEnd => "Move to end of text",
            Self::MoveUp => "Move up",
            Self::MoveDown => "Move down",
            Self::SelectLeft => "Select left",
            Self::SelectRight => "Select right",
            Self::SelectWordLeft => "Select to previous word",
            Self::SelectWordRight => "Select to next word",
            Self::SelectLineStart => "Select to line start",
            Self::SelectLineEnd => "Select to line end",
            Self::SelectStart => "Select to start of text",
            Self::SelectEnd => "Select to end of text",
            Self::SelectUp => "Select up",
            Self::SelectDown => "Select down",
            Self::SelectAll => "Select all",
            Self::DeleteBackward => "Delete previous character",
            Self::DeleteForward => "Delete next character",
            Self::DeleteWordBackward => "Delete previous word",
            Self::DeleteWordForward => "Delete next word",
            Self::DeleteToLineStart => "Delete to line start",
            Self::DeleteToLineEnd => "Delete to line end",
            Self::DeleteToStart => "Delete to start of text",
            Self::DeleteToEnd => "Delete to end of text",
            Self::Clear => "Clear text",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
        }
    }

    /// Return the cursor motion and selection mode for movement and selection actions.
    ///
    /// Vertical actions return [`TextMotion::VisualUp`] or
    /// [`TextMotion::VisualDown`]; layout-aware consumers can route those through
    /// their own measured projection (for example
    /// [`TextEditBuffer::move_cursor_in_layout`]) instead of
    /// [`TextEditBuffer::apply_action`]. [`SelectAll`](Self::SelectAll) is not a
    /// motion and returns `None`.
    #[must_use]
    pub const fn motion(self) -> Option<(TextMotion, SelectionMode)> {
        let (motion, mode) = match self {
            Self::MoveLeft => (TextMotion::Left, SelectionMode::Move),
            Self::MoveRight => (TextMotion::Right, SelectionMode::Move),
            Self::MoveWordLeft => (TextMotion::WordLeft, SelectionMode::Move),
            Self::MoveWordRight => (TextMotion::WordRight, SelectionMode::Move),
            Self::MoveLineStart => (TextMotion::LineStart, SelectionMode::Move),
            Self::MoveLineEnd => (TextMotion::LineEnd, SelectionMode::Move),
            Self::MoveStart => (TextMotion::Start, SelectionMode::Move),
            Self::MoveEnd => (TextMotion::End, SelectionMode::Move),
            Self::MoveUp => (TextMotion::VisualUp, SelectionMode::Move),
            Self::MoveDown => (TextMotion::VisualDown, SelectionMode::Move),
            Self::SelectLeft => (TextMotion::Left, SelectionMode::Extend),
            Self::SelectRight => (TextMotion::Right, SelectionMode::Extend),
            Self::SelectWordLeft => (TextMotion::WordLeft, SelectionMode::Extend),
            Self::SelectWordRight => (TextMotion::WordRight, SelectionMode::Extend),
            Self::SelectLineStart => (TextMotion::LineStart, SelectionMode::Extend),
            Self::SelectLineEnd => (TextMotion::LineEnd, SelectionMode::Extend),
            Self::SelectStart => (TextMotion::Start, SelectionMode::Extend),
            Self::SelectEnd => (TextMotion::End, SelectionMode::Extend),
            Self::SelectUp => (TextMotion::VisualUp, SelectionMode::Extend),
            Self::SelectDown => (TextMotion::VisualDown, SelectionMode::Extend),
            Self::SelectAll
            | Self::DeleteBackward
            | Self::DeleteForward
            | Self::DeleteWordBackward
            | Self::DeleteWordForward
            | Self::DeleteToLineStart
            | Self::DeleteToLineEnd
            | Self::DeleteToStart
            | Self::DeleteToEnd
            | Self::Clear
            | Self::Undo
            | Self::Redo => return None,
        };
        Some((motion, mode))
    }

    /// Return the deletion performed by deletion actions.
    #[must_use]
    pub const fn deletion(self) -> Option<TextDelete> {
        match self {
            Self::DeleteBackward => Some(TextDelete::Backward),
            Self::DeleteForward => Some(TextDelete::Forward),
            Self::DeleteWordBackward => Some(TextDelete::WordBackward),
            Self::DeleteWordForward => Some(TextDelete::WordForward),
            Self::DeleteToLineStart => Some(TextDelete::ToLineStart),
            Self::DeleteToLineEnd => Some(TextDelete::ToLineEnd),
            Self::DeleteToStart => Some(TextDelete::ToStart),
            Self::DeleteToEnd => Some(TextDelete::ToEnd),
            Self::MoveLeft
            | Self::MoveRight
            | Self::MoveWordLeft
            | Self::MoveWordRight
            | Self::MoveLineStart
            | Self::MoveLineEnd
            | Self::MoveStart
            | Self::MoveEnd
            | Self::MoveUp
            | Self::MoveDown
            | Self::SelectLeft
            | Self::SelectRight
            | Self::SelectWordLeft
            | Self::SelectWordRight
            | Self::SelectLineStart
            | Self::SelectLineEnd
            | Self::SelectStart
            | Self::SelectEnd
            | Self::SelectUp
            | Self::SelectDown
            | Self::SelectAll
            | Self::Clear
            | Self::Undo
            | Self::Redo => None,
        }
    }

    /// Return true for actions that extend or replace the selection.
    #[must_use]
    pub const fn is_selection(self) -> bool {
        matches!(self, Self::SelectAll) || matches!(self.motion(), Some((_, SelectionMode::Extend)))
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for TextEditAction {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.id())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for TextEditAction {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor;

        impl serde::de::Visitor<'_> for IdVisitor {
            type Value = TextEditAction;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a text edit action id such as \"move-word-left\"")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                TextEditAction::from_id(value)
                    .ok_or_else(|| E::invalid_value(serde::de::Unexpected::Str(value), &self))
            }
        }

        deserializer.deserialize_str(IdVisitor)
    }
}

impl TextEditBuffer {
    /// Apply a named editing action and return whether the text changed.
    ///
    /// Vertical actions ([`MoveUp`](TextEditAction::MoveUp),
    /// [`MoveDown`](TextEditAction::MoveDown), [`SelectUp`](TextEditAction::SelectUp)
    /// and [`SelectDown`](TextEditAction::SelectDown)) use unbounded-width
    /// [`TextMotion::VisualUp`]/[`TextMotion::VisualDown`] semantics, i.e. hard
    /// lines. Consumers with a soft-wrapped viewport should intercept them (see
    /// [`TextEditAction::motion`]) and call
    /// [`move_cursor_in_layout`](Self::move_cursor_in_layout) instead.
    pub fn apply_action(&mut self, action: TextEditAction) -> bool {
        match action {
            TextEditAction::SelectAll => {
                self.select_all();
                false
            }
            TextEditAction::Clear => {
                let changed = !self.is_empty();
                self.clear();
                changed
            }
            TextEditAction::Undo => self.undo(),
            TextEditAction::Redo => self.redo(),
            action => {
                if let Some((motion, mode)) = action.motion() {
                    self.move_cursor_with_selection(motion, mode);
                    false
                } else if let Some(deletion) = action.deletion() {
                    // Deletions only remove bytes, so a length change detects an edit.
                    let before = self.text().len();
                    self.delete(deletion);
                    self.text().len() != before
                } else {
                    false
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TextSelection;

    #[test]
    fn ids_round_trip_and_are_unique_kebab_case() {
        let mut seen = std::collections::BTreeSet::new();
        for (index, action) in TextEditAction::ALL.iter().copied().enumerate() {
            assert_eq!(TextEditAction::from_id(action.id()), Some(action));
            assert!(seen.insert(action.id()), "duplicate id {}", action.id());
            assert!(
                action
                    .id()
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch == '-'),
                "{} is not kebab-case",
                action.id()
            );
            assert_ne!(action.label(), "");
            // `ALL` must follow declaration order so it can index tables.
            assert_eq!(action as usize, index);
        }
        assert_eq!(TextEditAction::from_id("not-an-action"), None);
        assert_eq!(TextEditAction::from_id("Move-Left"), None);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_uses_stable_ids() {
        for action in TextEditAction::ALL.iter().copied() {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(json, format!("\"{}\"", action.id()));
            assert_eq!(
                serde_json::from_str::<TextEditAction>(&json).unwrap(),
                action
            );
        }
        assert!(serde_json::from_str::<TextEditAction>("\"bogus\"").is_err());
    }

    fn editor(text: &str, cursor: usize) -> TextEditBuffer {
        let mut editor = TextEditBuffer::from_text(text);
        editor.move_cursor(TextMotion::Absolute(cursor));
        editor
    }

    #[test]
    fn movement_actions_move_and_clear_selection() {
        let cases = [
            (TextEditAction::MoveLeft, 6),
            (TextEditAction::MoveRight, 8),
            (TextEditAction::MoveWordLeft, 6),
            (TextEditAction::MoveWordRight, 11),
            (TextEditAction::MoveLineStart, 6),
            (TextEditAction::MoveLineEnd, 11),
            (TextEditAction::MoveStart, 0),
            (TextEditAction::MoveEnd, 17),
            (TextEditAction::MoveUp, 1),
            (TextEditAction::MoveDown, 13),
        ];
        for (action, expected) in cases {
            let mut editor = editor("hello\nworld\nagain", 6);
            editor.move_cursor_with_selection(TextMotion::Right, SelectionMode::Extend);
            assert!(editor.selection().is_some());
            assert!(!editor.apply_action(action), "{action:?}");
            assert_eq!(editor.cursor_byte_index(), expected, "{action:?}");
            assert_eq!(editor.selection(), None, "{action:?}");
        }
    }

    #[test]
    fn selection_actions_extend_from_anchor() {
        let cases = [
            (TextEditAction::SelectLeft, 6),
            (TextEditAction::SelectRight, 8),
            (TextEditAction::SelectWordLeft, 6),
            (TextEditAction::SelectWordRight, 11),
            (TextEditAction::SelectLineStart, 6),
            (TextEditAction::SelectLineEnd, 11),
            (TextEditAction::SelectStart, 0),
            (TextEditAction::SelectEnd, 17),
            (TextEditAction::SelectUp, 1),
            (TextEditAction::SelectDown, 13),
        ];
        for (action, expected) in cases {
            let mut editor = editor("hello\nworld\nagain", 7);
            assert!(!editor.apply_action(action), "{action:?}");
            assert_eq!(editor.cursor_byte_index(), expected, "{action:?}");
            assert_eq!(
                editor.selection(),
                Some(TextSelection {
                    start: expected.min(7),
                    end: expected.max(7),
                }),
                "{action:?}"
            );
        }

        let mut editor = editor("one two three", 13);
        editor.apply_action(TextEditAction::SelectWordLeft);
        editor.apply_action(TextEditAction::SelectWordLeft);
        assert_eq!(editor.selected_text().as_deref(), Some("two three"));

        editor.apply_action(TextEditAction::SelectAll);
        assert_eq!(editor.selected_text().as_deref(), Some("one two three"));
    }

    #[test]
    fn deletion_actions_report_text_changes() {
        let cases = [
            (TextEditAction::DeleteBackward, "hello\nwrld\nagain"),
            (TextEditAction::DeleteForward, "hello\nwold\nagain"),
            (TextEditAction::DeleteWordBackward, "hello\nrld\nagain"),
            (TextEditAction::DeleteWordForward, "hello\nwo\nagain"),
            (TextEditAction::DeleteToLineStart, "hello\nrld\nagain"),
            (TextEditAction::DeleteToLineEnd, "hello\nwo\nagain"),
            (TextEditAction::DeleteToStart, "rld\nagain"),
            (TextEditAction::DeleteToEnd, "hello\nwo"),
            (TextEditAction::Clear, ""),
        ];
        for (action, expected) in cases {
            let mut editor = editor("hello\nworld\nagain", 8);
            assert!(editor.apply_action(action), "{action:?}");
            assert_eq!(editor.text(), expected, "{action:?}");
            assert!(editor.can_undo(), "{action:?}");
        }

        let mut editor = editor("", 0);
        for action in [
            TextEditAction::DeleteBackward,
            TextEditAction::DeleteForward,
            TextEditAction::DeleteToStart,
            TextEditAction::Clear,
        ] {
            assert!(!editor.apply_action(action), "{action:?}");
        }
    }

    #[test]
    fn deletion_actions_replace_active_selection() {
        let mut editor = editor("hello world", 0);
        editor.apply_action(TextEditAction::SelectWordRight);
        assert!(editor.apply_action(TextEditAction::DeleteForward));
        assert_eq!(editor.text(), " world");
    }

    #[test]
    fn undo_and_redo_actions_walk_history() {
        let mut editor = editor("hello world", 11);
        assert!(!editor.apply_action(TextEditAction::Undo));
        assert!(editor.apply_action(TextEditAction::DeleteWordBackward));
        assert_eq!(editor.text(), "hello ");
        assert!(editor.apply_action(TextEditAction::Undo));
        assert_eq!(editor.text(), "hello world");
        assert!(editor.apply_action(TextEditAction::Redo));
        assert_eq!(editor.text(), "hello ");
        assert!(!editor.apply_action(TextEditAction::Redo));
    }

    #[test]
    fn every_action_is_applicable() {
        for action in TextEditAction::ALL.iter().copied() {
            let mut editor = editor("ab\ncd", 3);
            let before = editor.text().to_string();
            let changed = editor.apply_action(action);
            assert_eq!(changed, editor.text() != before, "{action:?}");
        }
    }
}
