//! Sticky section headers: which header items pin at the viewport top for a
//! resolved position, and where.
//!
//! Sections are declared by the item source
//! ([`VirtualItemSource::section_header_level`] and
//! [`VirtualItemSource::section_parent`]); this module only computes geometry
//! from the measured index, so it never scans the collection.

use std::ops::Range;

use super::{MeasuredListIndex, VirtualItemSource};

/// Sticky section-header policy for a `VirtualListComponent`.
///
/// Pinned headers never cover the whole viewport: a header pins only when the
/// pinned region, including it, leaves at least one content row visible and
/// stays within [`Self::max_rows`]. Deeper headers that do not fit are not
/// pinned (they scroll normally), which is the narrow-viewport degradation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StickyHeaders {
    /// Whether section headers pin at all.
    pub enabled: bool,
    /// Maximum viewport rows the pinned headers may cover together.
    pub max_rows: u64,
}

impl StickyHeaders {
    /// Pin section headers, bounded only by the viewport (the default).
    #[must_use]
    pub const fn enabled() -> Self {
        Self {
            enabled: true,
            max_rows: u64::MAX,
        }
    }

    /// Never pin section headers; header items scroll like any other item.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            max_rows: 0,
        }
    }

    /// Return this policy with at most `rows` viewport rows of pinned headers.
    #[must_use]
    pub const fn max_rows(mut self, rows: u64) -> Self {
        self.max_rows = rows;
        self
    }

    /// Viewport rows pinned headers may cover in a viewport of
    /// `viewport_height` rows: `min(max_rows, viewport_height - 1)`, or zero
    /// when disabled.
    #[must_use]
    pub const fn row_limit(self, viewport_height: u64) -> u64 {
        if !self.enabled {
            return 0;
        }
        let available = viewport_height.saturating_sub(1);
        if self.max_rows < available {
            self.max_rows
        } else {
            available
        }
    }
}

impl Default for StickyHeaders {
    fn default() -> Self {
        Self::enabled()
    }
}

/// Deepest section nesting followed through
/// [`VirtualItemSource::section_parent`].
const MAX_SECTION_DEPTH: usize = 64;

/// One header pinned at the viewport top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PinnedHeader {
    /// Index row of the header item.
    pub(super) index: usize,
    /// Collection row where the header item naturally starts.
    pub(super) start: u64,
    /// Exact measured height of the header item.
    pub(super) height: u64,
    /// Viewport row of the header's first row. Below the region pinned by
    /// enclosing headers, or above it (possibly negative) while the next
    /// section pushes the header out.
    pub(super) local_y: i64,
    /// Viewport rows the pinned header covers: from the bottom of the
    /// enclosing pinned headers to `local_y + height`.
    pub(super) visible: Range<u64>,
}

/// Headers enclosing the item at `index`, outermost first, including the item
/// itself when it is a header. The chain is cut at the first parent that is
/// not an earlier header with a strictly lower level.
fn enclosing_chain<K, Src>(source: &Src, index: usize, len: usize) -> Vec<(usize, u16)>
where
    Src: VirtualItemSource<K> + ?Sized,
{
    let mut chain = Vec::new();
    let mut bound = source.section_header_level(index);
    if let Some(level) = bound {
        chain.push((index, level));
    }
    let mut current = index;
    while chain.len() < MAX_SECTION_DEPTH {
        let Some(parent) = source.section_parent(current) else {
            break;
        };
        if parent >= current || parent >= len {
            break;
        }
        let Some(level) = source.section_header_level(parent) else {
            break;
        };
        if bound.is_some_and(|bound| level >= bound) {
            break;
        }
        chain.push((parent, level));
        bound = Some(level);
        current = parent;
    }
    chain.reverse();
    chain
}

fn signed(row: u64) -> i64 {
    i64::try_from(row).unwrap_or(i64::MAX)
}

/// Headers pinned for a viewport whose first row is collection row `top`.
///
/// Level by level, the item at the first unpinned viewport row selects the
/// next enclosing header deeper than the last pinned one. That header pins
/// when it starts above that row and fits the row limit; the first header of
/// the next section at the same or an outer level pushes it up as it arrives.
/// The index rows must equal source positions (no splices pending).
pub(super) fn pinned_headers<K, Src>(
    index: &MeasuredListIndex<K>,
    source: &Src,
    top: u64,
    viewport_height: u64,
    sticky: StickyHeaders,
) -> Vec<PinnedHeader>
where
    K: Clone + Ord,
    Src: VirtualItemSource<K> + ?Sized,
{
    let mut pinned = Vec::new();
    let limit = sticky.row_limit(viewport_height);
    let len = index.len();
    if limit == 0 || len != source.len() {
        return pinned;
    }
    let total = index.total_height();
    let mut bottom = 0_u64;
    let mut level: Option<u16> = None;
    loop {
        let row = top.saturating_add(bottom);
        if row >= total {
            break;
        }
        let Some(item) = index.item_at_offset(row) else {
            break;
        };
        let chain = enclosing_chain(source, item, len);
        let Some(&(header, header_level)) = chain
            .iter()
            .find(|(_, candidate)| level.is_none_or(|level| *candidate > level))
        else {
            break;
        };
        let (Some(start), Some(measured)) = (index.item_offset(header), index.item(header)) else {
            break;
        };
        if start >= row {
            // The header is already in place below the pinned region.
            break;
        }
        let height = measured.height;
        level = Some(header_level);
        if height == 0 {
            continue;
        }
        if bottom.saturating_add(height) > limit {
            break;
        }
        let mut local_y = signed(bottom);
        let end = row.saturating_add(height);
        for next in item + 1..len {
            let Some(next_start) = index.item_offset(next) else {
                break;
            };
            if next_start >= end {
                break;
            }
            if source
                .section_header_level(next)
                .is_some_and(|next_level| next_level <= header_level)
            {
                local_y = signed(next_start - top).saturating_sub(signed(height));
                break;
            }
        }
        let visible_end = u64::try_from(local_y.saturating_add(signed(height))).unwrap_or(0);
        if visible_end <= bottom {
            break;
        }
        pinned.push(PinnedHeader {
            index: header,
            start,
            height,
            local_y,
            visible: bottom..visible_end,
        });
        bottom = visible_end;
    }
    pinned
}

/// Viewport rows the headers enclosing item `position` cover when that item
/// is revealed directly below them: the summed heights of its enclosing
/// headers (excluding the item itself), outermost first, while they fit the
/// row limit.
pub(super) fn inset_for<K, Src>(
    index: &MeasuredListIndex<K>,
    source: &Src,
    position: usize,
    viewport_height: u64,
    sticky: StickyHeaders,
) -> u64
where
    K: Clone + Ord,
    Src: VirtualItemSource<K> + ?Sized,
{
    let limit = sticky.row_limit(viewport_height);
    let len = index.len();
    if limit == 0 || len != source.len() || position >= len {
        return 0;
    }
    let mut inset = 0_u64;
    for (header, _) in enclosing_chain(source, position, len) {
        if header == position {
            break;
        }
        let Some(measured) = index.item(header) else {
            break;
        };
        let next = inset.saturating_add(measured.height);
        if next > limit {
            break;
        }
        inset = next;
    }
    inset
}

#[cfg(test)]
mod tests;
