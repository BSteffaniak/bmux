//! Retained row layout for virtualized lists.
//!
//! This module provides domain-agnostic retained row caching for terminal UI
//! lists whose entries may render to multiple rows. Applications provide stable
//! signatures and row builders; `RetainedListLayout` owns cache synchronization
//! and visible-row projection.

use crate::text::Line;

/// Cached list entry signature.
pub trait RetainedListSignature: Clone + PartialEq + Eq {}

impl<T> RetainedListSignature for T where T: Clone + PartialEq + Eq {}

/// Cached rows for a retained list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedListLayout<S> {
    entries: Vec<RetainedListEntry<S>>,
}

impl<S> RetainedListLayout<S> {
    /// Create an empty retained list layout.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<S> Default for RetainedListLayout<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RetainedListEntry<S> {
    signature: S,
    rows: Vec<Line>,
}

impl<S> RetainedListEntry<S> {
    const fn new(signature: S, rows: Vec<Line>) -> Self {
        Self { signature, rows }
    }

    const fn row_count(&self) -> usize {
        self.rows.len()
    }
}

/// A rendered row inside a retained list's global row space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetainedListLine {
    /// Global row index from the first list row.
    pub row_index: usize,
    /// Entry index containing this row.
    pub entry_index: usize,
    /// Row index inside the containing entry.
    pub row_in_entry: usize,
}

impl<S> RetainedListLayout<S>
where
    S: RetainedListSignature,
{
    /// Synchronize retained entries using stable signatures and lazy row builders.
    pub fn sync<Sig, Rows>(&mut self, len: usize, signature: Sig, rows: Rows)
    where
        Sig: Fn(usize) -> S,
        Rows: FnMut(usize) -> Vec<Line>,
    {
        sync_entries(&mut self.entries, len, signature, rows);
    }

    /// Clear all cached entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Return the number of cached entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether the list has no cached entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return total rendered row count.
    #[must_use]
    pub fn total_rows(&self) -> usize {
        self.entries.iter().map(RetainedListEntry::row_count).sum()
    }

    /// Return visible cached rows for a top-origin start row and viewport height.
    #[must_use]
    pub fn visible_lines_from_top(
        &self,
        start: usize,
        viewport_height: u16,
    ) -> Vec<RetainedListLine> {
        let total_rows = self.total_rows();
        let end = start
            .saturating_add(usize::from(viewport_height))
            .min(total_rows);
        let mut visible = Vec::new();
        let mut row_cursor = 0usize;
        for (entry_index, entry) in self.entries.iter().enumerate() {
            push_visible_for_entry(
                &mut visible,
                start,
                end,
                &mut row_cursor,
                entry_index,
                entry,
            );
        }
        visible
    }

    /// Return the rendered line for a visible retained-list row.
    #[must_use]
    pub fn line(&self, visible: RetainedListLine) -> Option<&Line> {
        self.entries
            .get(visible.entry_index)?
            .rows
            .get(visible.row_in_entry)
    }

    /// Return the global start row for a cached entry.
    #[must_use]
    pub fn entry_start_row(&self, entry_index: usize) -> Option<usize> {
        let mut row_cursor = 0usize;
        for (index, entry) in self.entries.iter().enumerate() {
            if index == entry_index {
                return Some(row_cursor);
            }
            row_cursor = row_cursor.saturating_add(entry.row_count());
        }
        None
    }
}

fn sync_entries<S, Sig, Rows>(
    entries: &mut Vec<RetainedListEntry<S>>,
    len: usize,
    signature: Sig,
    mut rows: Rows,
) where
    S: RetainedListSignature,
    Sig: Fn(usize) -> S,
    Rows: FnMut(usize) -> Vec<Line>,
{
    if entries.len() > len {
        entries.truncate(len);
    }
    for index in 0..len {
        let signature = signature(index);
        match entries.get_mut(index) {
            Some(entry) if entry.signature == signature => {}
            Some(entry) => {
                *entry = RetainedListEntry::new(signature, rows(index));
            }
            None => entries.push(RetainedListEntry::new(signature, rows(index))),
        }
    }
}

fn push_visible_for_entry<S>(
    visible: &mut Vec<RetainedListLine>,
    start: usize,
    end: usize,
    row_cursor: &mut usize,
    entry_index: usize,
    entry: &RetainedListEntry<S>,
) {
    let entry_start = *row_cursor;
    let entry_end = entry_start.saturating_add(entry.row_count());
    if entry_end > start && entry_start < end {
        let row_start = start.saturating_sub(entry_start);
        let row_end = end.saturating_sub(entry_start).min(entry.row_count());
        visible.extend((row_start..row_end).map(|row_in_entry| RetainedListLine {
            row_index: entry_start.saturating_add(row_in_entry),
            entry_index,
            row_in_entry,
        }));
    }
    *row_cursor = entry_end;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Line;

    #[test]
    fn retained_list_reuses_rows_when_signature_is_unchanged() {
        let mut layout = RetainedListLayout::default();
        let mut builds = 0usize;
        layout.sync(
            2,
            |index| format!("entry-{index}"),
            |index| {
                builds = builds.saturating_add(1);
                vec![Line::from(format!("row-{index}"))]
            },
        );
        layout.sync(
            2,
            |index| format!("entry-{index}"),
            |index| {
                builds = builds.saturating_add(1);
                vec![Line::from(format!("changed-{index}"))]
            },
        );

        assert_eq!(builds, 2);
        assert_eq!(layout.total_rows(), 2);
    }

    #[test]
    fn retained_list_reports_visible_multiline_rows() {
        let mut layout = RetainedListLayout::default();
        layout.sync(
            2,
            |index| index,
            |index| {
                vec![
                    Line::from(format!("{index}a")),
                    Line::from(format!("{index}b")),
                ]
            },
        );

        let visible = layout.visible_lines_from_top(1, 2);

        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].entry_index, 0);
        assert_eq!(visible[0].row_in_entry, 1);
        assert_eq!(visible[1].entry_index, 1);
        assert_eq!(visible[1].row_in_entry, 0);
    }
}
