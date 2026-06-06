//! Retained row layout for virtualized sectioned lists.

use crate::retained_list::{RetainedListLayout, RetainedListLine, RetainedListSignature};
use crate::text::Line;

/// Cached retained-list section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedListSection<K, S> {
    key: K,
    layout: RetainedListLayout<S>,
}

impl<K, S> RetainedListSection<K, S> {
    const fn new(key: K) -> Self {
        Self {
            key,
            layout: RetainedListLayout::new(),
        }
    }
}

/// Cached rows for a retained sectioned list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedSectionedListLayout<K, S> {
    sections: Vec<RetainedListSection<K, S>>,
}

impl<K, S> Default for RetainedSectionedListLayout<K, S> {
    fn default() -> Self {
        Self {
            sections: Vec::new(),
        }
    }
}

/// A rendered row inside a retained sectioned list's global row space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetainedSectionedListLine<K> {
    /// Global row index from the first section row.
    pub row_index: usize,
    /// Section key containing this row.
    pub section: K,
    /// Entry index within the containing section.
    pub entry_index: usize,
    /// Row index inside the containing entry.
    pub row_in_entry: usize,
}

impl<K, S> RetainedSectionedListLayout<K, S>
where
    K: Clone + PartialEq + Eq,
    S: RetainedListSignature,
{
    /// Create an empty sectioned retained list layout.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sections: Vec::new(),
        }
    }

    /// Synchronize the set and order of sections.
    pub fn sync_sections(&mut self, keys: impl IntoIterator<Item = K>) {
        let keys = keys.into_iter().collect::<Vec<_>>();
        self.sections.retain(|section| keys.contains(&section.key));
        for (index, key) in keys.into_iter().enumerate() {
            if let Some(current_index) = self.sections.iter().position(|section| section.key == key)
            {
                let section = self.sections.remove(current_index);
                self.sections.insert(index, section);
            } else {
                self.sections.insert(index, RetainedListSection::new(key));
            }
        }
    }

    /// Synchronize entries in a section, creating the section if necessary at the end.
    pub fn sync_section<Sig, Rows>(&mut self, key: &K, len: usize, signature: Sig, rows: Rows)
    where
        Sig: Fn(usize) -> S,
        Rows: FnMut(usize) -> Vec<Line>,
    {
        if self.section_mut(key).is_none() {
            self.sections.push(RetainedListSection::new(key.clone()));
        }
        if let Some(section) = self.section_mut(key) {
            section.layout.sync(len, signature, rows);
        }
    }

    /// Clear a section if it exists.
    pub fn clear_section(&mut self, key: &K) {
        if let Some(section) = self.section_mut(key) {
            section.layout.clear();
        }
    }

    /// Clear all sections and cached rows.
    pub fn clear(&mut self) {
        self.sections.clear();
    }

    /// Return total rendered row count.
    #[must_use]
    pub fn total_rows(&self) -> usize {
        self.sections
            .iter()
            .map(|section| section.layout.total_rows())
            .sum()
    }

    /// Return visible cached rows for a top-origin start row and viewport height.
    #[must_use]
    pub fn visible_lines_from_top(
        &self,
        start: usize,
        viewport_height: u16,
    ) -> Vec<RetainedSectionedListLine<K>> {
        let total_rows = self.total_rows();
        let end = start
            .saturating_add(usize::from(viewport_height))
            .min(total_rows);
        let mut visible = Vec::new();
        let mut row_cursor = 0usize;
        for section in &self.sections {
            let section_start = row_cursor;
            let section_rows = section.layout.total_rows();
            let section_end = section_start.saturating_add(section_rows);
            if section_end > start && section_start < end {
                let local_start = start.saturating_sub(section_start);
                let local_height = end.saturating_sub(section_start).min(section_rows);
                visible.extend(
                    section
                        .layout
                        .visible_lines_from_top(local_start, saturating_u16(local_height))
                        .into_iter()
                        .map(|line| RetainedSectionedListLine {
                            row_index: section_start.saturating_add(line.row_index),
                            section: section.key.clone(),
                            entry_index: line.entry_index,
                            row_in_entry: line.row_in_entry,
                        }),
                );
            }
            row_cursor = section_end;
        }
        visible
    }

    /// Return the rendered line for a visible sectioned-list row.
    #[must_use]
    pub fn line(&self, visible: &RetainedSectionedListLine<K>) -> Option<&Line> {
        self.section(&visible.section)?
            .layout
            .line(RetainedListLine {
                row_index: visible.row_index,
                entry_index: visible.entry_index,
                row_in_entry: visible.row_in_entry,
            })
    }

    /// Return the global start row for a cached section entry.
    #[must_use]
    pub fn entry_start_row(&self, key: &K, entry_index: usize) -> Option<usize> {
        let mut row_cursor = 0usize;
        for section in &self.sections {
            if &section.key == key {
                return section
                    .layout
                    .entry_start_row(entry_index)
                    .map(|start| start.saturating_add(row_cursor));
            }
            row_cursor = row_cursor.saturating_add(section.layout.total_rows());
        }
        None
    }

    fn section(&self, key: &K) -> Option<&RetainedListSection<K, S>> {
        self.sections.iter().find(|section| &section.key == key)
    }

    fn section_mut(&mut self, key: &K) -> Option<&mut RetainedListSection<K, S>> {
        self.sections.iter_mut().find(|section| &section.key == key)
    }
}

fn saturating_u16(value: usize) -> u16 {
    u16::try_from(value.min(usize::from(u16::MAX))).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Line;

    #[test]
    fn sectioned_list_reports_visible_rows_across_sections() {
        let mut layout = RetainedSectionedListLayout::new();
        layout.sync_sections(["a", "b"]);
        layout.sync_section(&"a", 1, |_| 0, |_| vec![Line::from("a0"), Line::from("a1")]);
        layout.sync_section(&"b", 1, |_| 0, |_| vec![Line::from("b0"), Line::from("b1")]);

        let visible = layout.visible_lines_from_top(1, 2);

        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].section, "a");
        assert_eq!(visible[0].row_in_entry, 1);
        assert_eq!(visible[1].section, "b");
        assert_eq!(visible[1].row_in_entry, 0);
    }

    #[test]
    fn sectioned_list_offsets_entry_start_rows() {
        let mut layout = RetainedSectionedListLayout::new();
        layout.sync_sections(["a", "b"]);
        layout.sync_section(&"a", 1, |_| 0, |_| vec![Line::from("a0"), Line::from("a1")]);
        layout.sync_section(&"b", 1, |_| 0, |_| vec![Line::from("b0")]);

        assert_eq!(layout.entry_start_row(&"a", 0), Some(0));
        assert_eq!(layout.entry_start_row(&"b", 0), Some(2));
    }
}
