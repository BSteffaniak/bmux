//! Filtered-list selection projection state.
//!
//! This module intentionally stores only source-index projection and selection
//! state. Rendering and item semantics stay caller-owned so the state can be
//! composed with [`crate::selectable_list`], picker frames, or custom lists.

/// Selection state for a filtered projection of caller-owned source items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredListState {
    filtered_indices: Vec<usize>,
    selected_visible: Option<usize>,
}

impl FilteredListState {
    /// Create state with all source indices visible.
    #[must_use]
    pub fn new(item_count: usize) -> Self {
        let filtered_indices = (0..item_count).collect::<Vec<_>>();
        let selected_visible = (!filtered_indices.is_empty()).then_some(0);
        Self {
            filtered_indices,
            selected_visible,
        }
    }

    /// Create state from explicit source indices.
    #[must_use]
    pub fn from_indices(filtered_indices: impl Into<Vec<usize>>) -> Self {
        let filtered_indices = filtered_indices.into();
        let selected_visible = (!filtered_indices.is_empty()).then_some(0);
        Self {
            filtered_indices,
            selected_visible,
        }
    }

    /// Return visible source indices.
    #[must_use]
    pub fn indices(&self) -> &[usize] {
        &self.filtered_indices
    }

    /// Return selected visible row index.
    #[must_use]
    pub const fn selected_visible(&self) -> Option<usize> {
        self.selected_visible
    }

    /// Return selected source item index.
    #[must_use]
    pub fn selected_source_index(&self) -> Option<usize> {
        self.selected_visible
            .and_then(|visible| self.filtered_indices.get(visible).copied())
    }

    /// Replace visible source indices and keep selection valid.
    pub fn replace_indices(&mut self, filtered_indices: impl Into<Vec<usize>>) {
        self.filtered_indices = filtered_indices.into();
        self.selected_visible = if self.filtered_indices.is_empty() {
            None
        } else {
            Some(
                self.selected_visible
                    .unwrap_or(0)
                    .min(self.filtered_indices.len().saturating_sub(1)),
            )
        };
    }

    /// Move selection to the next visible row.
    pub fn select_next(&mut self) -> bool {
        let Some(selected) = self.selected_visible else {
            return false;
        };
        let next = selected
            .saturating_add(1)
            .min(self.filtered_indices.len().saturating_sub(1));
        if next == selected {
            false
        } else {
            self.selected_visible = Some(next);
            true
        }
    }

    /// Move selection to the previous visible row.
    pub const fn select_previous(&mut self) -> bool {
        let Some(selected) = self.selected_visible else {
            return false;
        };
        let previous = selected.saturating_sub(1);
        if previous == selected {
            false
        } else {
            self.selected_visible = Some(previous);
            true
        }
    }

    /// Select a visible row by zero-based index.
    pub const fn select_visible(&mut self, visible: usize) -> bool {
        if visible >= self.filtered_indices.len() {
            return false;
        }
        self.selected_visible = Some(visible);
        true
    }

    /// Clear selection while preserving visible source indices.
    pub const fn clear_selection(&mut self) {
        self.selected_visible = None;
    }
}

#[cfg(test)]
mod tests {
    use super::FilteredListState;

    #[test]
    fn new_exposes_all_indices_and_selects_first() {
        let state = FilteredListState::new(3);

        assert_eq!(state.indices(), &[0, 1, 2]);
        assert_eq!(state.selected_visible(), Some(0));
        assert_eq!(state.selected_source_index(), Some(0));
    }

    #[test]
    fn new_with_zero_items_has_no_selection() {
        let state = FilteredListState::new(0);

        assert!(state.indices().is_empty());
        assert_eq!(state.selected_visible(), None);
        assert_eq!(state.selected_source_index(), None);
    }

    #[test]
    fn replace_indices_clamps_selection() {
        let mut state = FilteredListState::new(5);
        assert!(state.select_visible(4));

        state.replace_indices([2, 4]);

        assert_eq!(state.indices(), &[2, 4]);
        assert_eq!(state.selected_visible(), Some(1));
        assert_eq!(state.selected_source_index(), Some(4));
    }

    #[test]
    fn replace_indices_clears_selection_when_empty() {
        let mut state = FilteredListState::new(3);

        state.replace_indices([]);

        assert!(state.indices().is_empty());
        assert_eq!(state.selected_visible(), None);
    }

    #[test]
    fn visible_row_selection_maps_to_source_index() {
        let mut state = FilteredListState::from_indices([4, 8, 12]);

        assert!(state.select_visible(2));
        assert_eq!(state.selected_visible(), Some(2));
        assert_eq!(state.selected_source_index(), Some(12));
        assert!(!state.select_visible(3));
        assert_eq!(state.selected_visible(), Some(2));
    }

    #[test]
    fn next_and_previous_navigation_are_clamped() {
        let mut state = FilteredListState::from_indices([10, 20]);

        assert_eq!(state.selected_source_index(), Some(10));
        assert!(state.select_next());
        assert_eq!(state.selected_source_index(), Some(20));
        assert!(!state.select_next());
        assert!(state.select_previous());
        assert_eq!(state.selected_source_index(), Some(10));
        assert!(!state.select_previous());
    }

    #[test]
    fn cleared_selection_does_not_move() {
        let mut state = FilteredListState::new(3);

        state.clear_selection();

        assert!(!state.select_next());
        assert!(!state.select_previous());
        assert_eq!(state.selected_source_index(), None);
    }
}
