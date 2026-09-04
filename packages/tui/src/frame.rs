//! Frame render context.

use crate::buffer::Buffer;
use crate::damage::{Damage, DamagePolicy};
use crate::focus::FocusScopeId;
use crate::geometry::{Point, Rect};
use crate::hit::{HitMap, HitRegion};
use crate::image::ImageContribution;
use crate::selection::{
    SelectionFragment, SelectionScene, SelectionScope, SelectionSnapshot,
    paint_selection_highlights,
};
use crate::semantic::{SemanticRegion, SemanticScene};
use crate::style::Style;
use crate::text::Line;

/// Cursor requested by a rendered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Cursor position.
    pub position: Point,
    /// Whether the cursor should be visible.
    pub visible: bool,
}

impl Cursor {
    /// Create a visible cursor at `position`.
    #[must_use]
    pub const fn visible(position: Point) -> Self {
        Self {
            position,
            visible: true,
        }
    }

    /// Create a hidden cursor at `position`.
    #[must_use]
    pub const fn hidden(position: Point) -> Self {
        Self {
            position,
            visible: false,
        }
    }
}

/// Staging container for one frame's cells and interaction metadata.
///
/// A frame owns the buffer being painted plus the cursor, hit, focus-scope,
/// image, selection, semantic, and damage contributions registered while
/// painting. It never accepts writes directly: every contribution flows
/// through the scoped [`crate::paint::PaintCx`] created over it, which
/// applies translation and clipping uniformly. Presenters and offscreen
/// staging code create a frame, paint through a root `PaintCx`, and then read
/// the committed metadata through this type's accessors.
pub struct Frame<'buffer> {
    buffer: &'buffer mut Buffer,
    cursor: Option<Cursor>,
    hits: HitMap,
    focus_scope: Option<FocusScopeId>,
    images: Vec<ImageContribution>,
    selection: SelectionScene,
    semantics: SemanticScene,
    damage: Vec<Rect>,
}

impl<'buffer> Frame<'buffer> {
    /// Create a frame that stages painting into `buffer`.
    pub const fn new(buffer: &'buffer mut Buffer) -> Self {
        Self {
            buffer,
            cursor: None,
            hits: HitMap::new(),
            focus_scope: None,
            images: Vec::new(),
            selection: SelectionScene::new(),
            semantics: SemanticScene::new(),
            damage: Vec::new(),
        }
    }

    /// Return the frame area.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.buffer.area()
    }

    /// Return the current cursor request.
    #[must_use]
    pub const fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }

    /// Return an immutable view of the backing buffer.
    #[must_use]
    pub const fn buffer(&self) -> &Buffer {
        self.buffer
    }

    /// Return registered hit regions.
    #[must_use]
    pub const fn hits(&self) -> &HitMap {
        &self.hits
    }

    /// Return the active focus scope requested by this frame.
    #[must_use]
    pub const fn focus_scope(&self) -> Option<&FocusScopeId> {
        self.focus_scope.as_ref()
    }

    /// Return image lifecycle contributions registered for this frame.
    #[must_use]
    pub fn images(&self) -> &[ImageContribution] {
        &self.images
    }

    /// Return selection metadata registered for this frame.
    #[must_use]
    pub const fn selection(&self) -> &SelectionScene {
        &self.selection
    }

    /// Return semantic regions registered for this frame.
    #[must_use]
    pub const fn semantics(&self) -> &SemanticScene {
        &self.semantics
    }

    /// Return bounded damage requested by rendered components.
    #[must_use]
    pub fn damage(&self, policy: DamagePolicy) -> Damage {
        Damage::regions(self.damage.iter().copied(), self.area(), policy)
    }

    // Every mutator below is reachable only through `crate::paint::PaintCx`,
    // which translates and clips each contribution before staging it here.

    pub(crate) const fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }

    pub(crate) const fn set_cursor(&mut self, cursor: Cursor) {
        self.cursor = Some(cursor);
    }

    pub(crate) fn push_hit(&mut self, mut region: HitRegion) {
        if let Some(scope) = self.focus_scope.as_ref()
            && region.focusable
            && region.focus_scope.is_none()
        {
            region.focus_scope = Some(scope.clone());
        }
        self.hits.push(region);
    }

    pub(crate) fn set_focus_scope(&mut self, scope: Option<FocusScopeId>) {
        self.focus_scope = scope;
    }

    pub(crate) fn push_image(&mut self, contribution: ImageContribution) {
        self.images.push(contribution);
    }

    pub(crate) fn push_semantic(&mut self, region: SemanticRegion) {
        self.semantics.push(region);
    }

    pub(crate) fn push_damage(&mut self, area: Rect) {
        if !area.is_empty() {
            self.damage.push(area);
        }
    }

    pub(crate) fn push_selection_scope(&mut self, scope: SelectionScope) {
        self.selection.push_scope(scope);
    }

    pub(crate) fn push_selection_fragment(&mut self, fragment: SelectionFragment) {
        self.selection.push_fragment(fragment);
    }

    pub(crate) fn paint_selection(&mut self, snapshot: &SelectionSnapshot, style: Style) {
        paint_selection_highlights(self.buffer, &snapshot.visible_highlights, style);
    }

    pub(crate) fn fill(&mut self, area: Rect, symbol: &str, style: Style) {
        self.buffer.fill(area, symbol, style);
    }

    pub(crate) fn write_line_with_fallback_style(&mut self, area: Rect, line: &Line, style: Style) {
        self.buffer
            .write_line_with_fallback_style(area, line, style);
    }
}

#[cfg(test)]
mod tests {
    use super::{Cursor, Frame};
    use crate::buffer::Buffer;
    use crate::geometry::{Point, Rect};
    use crate::image::{ImageContribution, ImageKey};

    #[test]
    fn frame_tracks_cursor_request() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
        let mut frame = Frame::new(&mut buffer);

        frame.set_cursor(Cursor::visible(Point::new(1, 1)));

        assert_eq!(frame.cursor(), Some(Cursor::visible(Point::new(1, 1))));
    }

    #[test]
    fn frame_collects_image_contributions_in_render_order() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
        let mut frame = Frame::new(&mut buffer);

        frame.push_image(ImageContribution::Remove(ImageKey::new("first")));
        frame.push_image(ImageContribution::Remove(ImageKey::new("second")));

        assert_eq!(
            frame.images(),
            [
                ImageContribution::Remove(ImageKey::new("first")),
                ImageContribution::Remove(ImageKey::new("second")),
            ]
        );
    }
}
