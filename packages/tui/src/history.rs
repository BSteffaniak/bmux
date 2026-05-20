//! Text input history navigation helpers.

use bmux_text_edit::TextEditBuffer;

/// Direction for text input history navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputHistoryDirection {
    /// Navigate to an older entry.
    Previous,
    /// Navigate to a newer entry or back to the draft.
    Next,
}

/// Immutable text input history entries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextInputHistory {
    entries: Vec<String>,
}

impl TextInputHistory {
    /// Create an empty history.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Create history from entries ordered oldest to newest.
    #[must_use]
    pub fn from_entries(entries: impl Into<Vec<String>>) -> Self {
        Self {
            entries: entries.into(),
        }
    }

    /// Add an entry to the end of history.
    pub fn push(&mut self, entry: impl Into<String>) {
        let entry = entry.into();
        if !entry.is_empty() {
            self.entries.push(entry);
        }
    }

    /// Return all history entries, ordered oldest to newest.
    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Return true when history is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return history length.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Navigate history and replace `buffer` when movement is possible.
    pub fn navigate(
        &self,
        state: &mut TextInputHistoryState,
        buffer: &mut TextEditBuffer,
        direction: TextInputHistoryDirection,
    ) -> bool {
        if self.entries.is_empty() {
            return false;
        }

        match direction {
            TextInputHistoryDirection::Previous => self.previous(state, buffer),
            TextInputHistoryDirection::Next => self.next(state, buffer),
        }
    }

    fn previous(&self, state: &mut TextInputHistoryState, buffer: &mut TextEditBuffer) -> bool {
        let next_index = match state.index {
            Some(0) => 0,
            Some(index) => index.saturating_sub(1),
            None => {
                state.draft = Some(buffer.text().to_owned());
                self.entries.len().saturating_sub(1)
            }
        };
        state.index = Some(next_index);
        replace_buffer(buffer, &self.entries[next_index]);
        true
    }

    fn next(&self, state: &mut TextInputHistoryState, buffer: &mut TextEditBuffer) -> bool {
        let Some(index) = state.index else {
            return false;
        };
        if index.saturating_add(1) < self.entries.len() {
            let next_index = index.saturating_add(1);
            state.index = Some(next_index);
            replace_buffer(buffer, &self.entries[next_index]);
            return true;
        }

        let draft = state.draft.take().unwrap_or_default();
        state.index = None;
        replace_buffer(buffer, &draft);
        true
    }
}

/// Mutable history navigation state for one text input.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextInputHistoryState {
    index: Option<usize>,
    draft: Option<String>,
}

impl TextInputHistoryState {
    /// Create empty history navigation state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            index: None,
            draft: None,
        }
    }

    /// Return active history index, if navigating history.
    #[must_use]
    pub const fn index(&self) -> Option<usize> {
        self.index
    }

    /// Reset navigation state and forget the saved draft.
    pub fn reset(&mut self) {
        self.index = None;
        self.draft = None;
    }
}

fn replace_buffer(buffer: &mut TextEditBuffer, text: &str) {
    *buffer = TextEditBuffer::from_text(text);
}

#[cfg(test)]
mod tests {
    use super::{TextInputHistory, TextInputHistoryDirection, TextInputHistoryState};
    use bmux_text_edit::TextEditBuffer;

    #[test]
    fn history_previous_loads_newest_entry_and_preserves_draft() {
        let history = TextInputHistory::from_entries(vec!["one".to_owned(), "two".to_owned()]);
        let mut state = TextInputHistoryState::new();
        let mut buffer = TextEditBuffer::from_text("draft");

        assert!(history.navigate(&mut state, &mut buffer, TextInputHistoryDirection::Previous));

        assert_eq!(buffer.text(), "two");
        assert_eq!(state.index(), Some(1));
    }

    #[test]
    fn history_previous_and_next_cycle_entries_and_draft() {
        let history = TextInputHistory::from_entries(vec!["one".to_owned(), "two".to_owned()]);
        let mut state = TextInputHistoryState::new();
        let mut buffer = TextEditBuffer::from_text("draft");

        history.navigate(&mut state, &mut buffer, TextInputHistoryDirection::Previous);
        history.navigate(&mut state, &mut buffer, TextInputHistoryDirection::Previous);
        assert_eq!(buffer.text(), "one");
        assert_eq!(state.index(), Some(0));

        history.navigate(&mut state, &mut buffer, TextInputHistoryDirection::Next);
        assert_eq!(buffer.text(), "two");
        assert_eq!(state.index(), Some(1));

        history.navigate(&mut state, &mut buffer, TextInputHistoryDirection::Next);
        assert_eq!(buffer.text(), "draft");
        assert_eq!(state.index(), None);
    }

    #[test]
    fn history_next_without_active_navigation_is_ignored() {
        let history = TextInputHistory::from_entries(vec!["one".to_owned()]);
        let mut state = TextInputHistoryState::new();
        let mut buffer = TextEditBuffer::from_text("draft");

        assert!(!history.navigate(&mut state, &mut buffer, TextInputHistoryDirection::Next));
        assert_eq!(buffer.text(), "draft");
    }
}
