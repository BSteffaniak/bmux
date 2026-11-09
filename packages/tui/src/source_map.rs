//! Caller source positions mapped to rows of one laid-out content.
//!
//! Reflow (a width change, a style change that alters wrapping, streamed
//! appends) moves content between rows. A reader's position survives it only
//! when it is stored as a position in the *source* and mapped back to a row
//! after the new layout. [`SourceMapped`] is that mapping for a single content
//! block at its current layout. The position type `S` is opaque: BMUX never
//! interprets it, only compares rows.
//!
//! [`TextBlock::source_rows`](crate::composition::TextBlock::source_rows)
//! implements it over UTF-8 byte offsets from a retained text layout. Keyed
//! collections (for example `bmux_tui_components::virtual_list::SourceMap`)
//! usually delegate to one `SourceMapped` per item, and
//! `bmux_tui_components::ScrollView` restores source anchors through it.

/// Maps opaque source positions to rows of one content at its current layout.
///
/// Implementations must be consistent: when `source_for_row(row)` returns
/// `Some((position, start))`, then `start <= row` and
/// `row_for_source(&position)` returns `Some(start)`.
pub trait SourceMapped<S> {
    /// Row, at the current layout, where `position` begins, or `None` when the
    /// position is not part of this content.
    fn row_for_source(&self, position: &S) -> Option<u64>;

    /// Source position covering `row`, with the row where that position
    /// begins (at most `row`), or `None` when the row is not mapped.
    fn source_for_row(&self, row: u64) -> Option<(S, u64)>;
}

impl<S, T: SourceMapped<S> + ?Sized> SourceMapped<S> for &T {
    fn row_for_source(&self, position: &S) -> Option<u64> {
        (**self).row_for_source(position)
    }

    fn source_for_row(&self, row: u64) -> Option<(S, u64)> {
        (**self).source_for_row(row)
    }
}

/// A [`SourceMapped`] whose rows start `origin` rows below another one's.
///
/// Use it when a mapped block is placed inside larger content, for example a
/// text block below a header inside a scroll view, so rows are expressed in
/// the enclosing content's coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffsetSourceRows<M> {
    inner: M,
    origin: u64,
}

impl<M> OffsetSourceRows<M> {
    /// Offset every row of `inner` by `origin`.
    #[must_use]
    pub const fn new(inner: M, origin: u64) -> Self {
        Self { inner, origin }
    }
}

impl<S, M: SourceMapped<S>> SourceMapped<S> for OffsetSourceRows<M> {
    fn row_for_source(&self, position: &S) -> Option<u64> {
        self.inner
            .row_for_source(position)
            .map(|row| row.saturating_add(self.origin))
    }

    fn source_for_row(&self, row: u64) -> Option<(S, u64)> {
        let local = row.checked_sub(self.origin)?;
        self.inner
            .source_for_row(local)
            .map(|(position, start)| (position, start.saturating_add(self.origin)))
    }
}
