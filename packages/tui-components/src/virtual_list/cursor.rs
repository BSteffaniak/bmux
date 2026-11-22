//! A caller-owned item cursor and hover for keyed lists.
//!
//! The list never owns focus or selection (see [`super::focus`]); this is the
//! small piece of state most list callers otherwise hand-roll: which item the
//! keyboard is on, which item the pointer is over, and how cursor keys move
//! between items. Callers style the cursor and hovered items when they build
//! the list, and reveal the cursor item after it moves.

/// A cursor movement or activation, mapped by the caller from its own keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorInput {
    /// The previous item.
    Up,
    /// The next item.
    Down,
    /// The first item.
    Home,
    /// The last item.
    End,
    /// This many items back (a page).
    PageUp(usize),
    /// This many items forward (a page).
    PageDown(usize),
    /// Activate the cursor item.
    Activate,
}

/// What a [`CursorInput`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorOutcome<K> {
    /// Nothing changed (no items, or already at the edge without wrapping).
    Unchanged,
    /// The cursor moved to this item.
    Moved(K),
    /// The cursor item was activated.
    Activated(K),
}

/// Which item the keyboard is on and which the pointer is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemCursor<K> {
    cursor: Option<K>,
    hovered: Option<K>,
    wrap: bool,
}

impl<K> Default for ItemCursor<K> {
    fn default() -> Self {
        Self {
            cursor: None,
            hovered: None,
            wrap: false,
        }
    }
}

impl<K: Clone + PartialEq> ItemCursor<K> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Move past the last item to the first, and before the first to the last.
    #[must_use]
    pub const fn wrapping(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// The item the keyboard is on.
    #[must_use]
    pub const fn cursor(&self) -> Option<&K> {
        self.cursor.as_ref()
    }

    /// The item the pointer is over.
    #[must_use]
    pub const fn hovered(&self) -> Option<&K> {
        self.hovered.as_ref()
    }

    /// Put the keyboard on `key`. Returns whether it changed.
    pub fn set_cursor(&mut self, key: Option<K>) -> bool {
        let changed = self.cursor != key;
        self.cursor = key;
        changed
    }

    /// Record the item under the pointer. Returns whether it changed.
    pub fn set_hovered(&mut self, key: Option<K>) -> bool {
        let changed = self.hovered != key;
        self.hovered = key;
        changed
    }

    /// Apply `input` over `keys` (the list's keys in order). A cursor that is
    /// not among `keys` starts from the first item (or the last for `Up`,
    /// `End` and `PageUp`).
    pub fn apply(&mut self, keys: &[K], input: CursorInput) -> CursorOutcome<K> {
        let Some(last) = keys.len().checked_sub(1) else {
            return CursorOutcome::Unchanged;
        };
        let current = self
            .cursor
            .as_ref()
            .and_then(|cursor| keys.iter().position(|key| key == cursor));
        let target = match (input, current) {
            (CursorInput::Activate, Some(index)) => {
                return CursorOutcome::Activated(keys[index].clone());
            }
            (CursorInput::Activate, None) => return CursorOutcome::Unchanged,
            (CursorInput::Home, _) | (CursorInput::Down | CursorInput::PageDown(_), None) => 0,
            (CursorInput::End, _) | (CursorInput::Up | CursorInput::PageUp(_), None) => last,
            (CursorInput::Up, Some(0)) if self.wrap => last,
            (CursorInput::Down, Some(index)) if index == last && self.wrap => 0,
            (CursorInput::Up, Some(index)) => index.saturating_sub(1),
            (CursorInput::Down, Some(index)) => index.saturating_add(1).min(last),
            (CursorInput::PageUp(step), Some(index)) => index.saturating_sub(step.max(1)),
            (CursorInput::PageDown(step), Some(index)) => {
                index.saturating_add(step.max(1)).min(last)
            }
        };
        if current == Some(target) {
            return CursorOutcome::Unchanged;
        }
        self.cursor = Some(keys[target].clone());
        CursorOutcome::Moved(keys[target].clone())
    }

    /// Drop the cursor and hover from items that are no longer listed.
    pub fn retain(&mut self, keys: &[K]) {
        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| !keys.contains(cursor))
        {
            self.cursor = None;
        }
        if self
            .hovered
            .as_ref()
            .is_some_and(|hovered| !keys.contains(hovered))
        {
            self.hovered = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_keys_move_between_items_and_activate() {
        let keys = ['a', 'b', 'c', 'd'];
        let mut cursor = ItemCursor::new();
        assert_eq!(
            cursor.apply(&keys, CursorInput::Activate),
            CursorOutcome::Unchanged
        );
        assert_eq!(
            cursor.apply(&keys, CursorInput::Down),
            CursorOutcome::Moved('a')
        );
        assert_eq!(
            cursor.apply(&keys, CursorInput::Down),
            CursorOutcome::Moved('b')
        );
        assert_eq!(
            cursor.apply(&keys, CursorInput::PageDown(5)),
            CursorOutcome::Moved('d')
        );
        assert_eq!(
            cursor.apply(&keys, CursorInput::Down),
            CursorOutcome::Unchanged
        );
        assert_eq!(
            cursor.apply(&keys, CursorInput::Home),
            CursorOutcome::Moved('a')
        );
        assert_eq!(
            cursor.apply(&keys, CursorInput::Up),
            CursorOutcome::Unchanged
        );
        assert_eq!(
            cursor.apply(&keys, CursorInput::Activate),
            CursorOutcome::Activated('a')
        );
        let mut wrapping = ItemCursor::new().wrapping(true);
        assert_eq!(
            wrapping.apply(&keys, CursorInput::Up),
            CursorOutcome::Moved('d')
        );
        assert_eq!(
            wrapping.apply(&keys, CursorInput::Down),
            CursorOutcome::Moved('a')
        );
        assert_eq!(
            cursor.apply(&[], CursorInput::Down),
            CursorOutcome::Unchanged
        );
    }

    #[test]
    fn hover_and_cursor_are_dropped_with_their_items() {
        let mut cursor = ItemCursor::new();
        assert!(cursor.set_cursor(Some(2)));
        assert!(!cursor.set_cursor(Some(2)));
        assert!(cursor.set_hovered(Some(3)));
        cursor.retain(&[1, 2]);
        assert_eq!(cursor.cursor(), Some(&2));
        assert_eq!(cursor.hovered(), None);
        // A cursor that left the list starts again from the edge.
        cursor.retain(&[1]);
        assert_eq!(
            cursor.apply(&[1, 5], CursorInput::Down),
            CursorOutcome::Moved(1)
        );
    }
}
