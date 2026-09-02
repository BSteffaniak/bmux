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

/// Mutable render context for a single frame.
pub struct Frame<'buffer> {
    buffer: &'buffer mut Buffer,
    cursor: Option<Cursor>,
    hits: HitMap,
    automatic_hit_index: usize,
    focus_scope: Option<FocusScopeId>,
    images: Vec<ImageContribution>,
    selection: SelectionScene,
    semantics: SemanticScene,
    damage: Vec<Rect>,
}

impl<'buffer> Frame<'buffer> {
    /// Create a frame that renders into `buffer`.
    pub const fn new(buffer: &'buffer mut Buffer) -> Self {
        Self {
            buffer,
            cursor: None,
            hits: HitMap::new(),
            automatic_hit_index: 0,
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

    /// Request a cursor state for this frame.
    pub const fn set_cursor(&mut self, cursor: Cursor) {
        self.cursor = Some(cursor);
    }

    /// Return an immutable view of the backing buffer.
    #[must_use]
    pub const fn buffer(&self) -> &Buffer {
        self.buffer
    }

    /// Return mutable access to the backing buffer for the canonical scoped
    /// paint implementation. Components and downstream consumers must use
    /// [`crate::paint::PaintCx`] instead.
    pub(crate) const fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }

    /// Return registered hit regions.
    #[must_use]
    pub const fn hits(&self) -> &HitMap {
        &self.hits
    }

    /// Add a hit-test region for this frame.
    pub fn push_hit(&mut self, mut region: HitRegion) {
        if let Some(scope) = self.focus_scope.as_ref()
            && region.focusable
            && region.focus_scope.is_none()
        {
            region.focus_scope = Some(scope.clone());
        }
        self.hits.push(region);
    }

    /// Create a deterministic render-order identifier for an automatic control.
    pub fn next_interaction_id(&mut self, kind: &str) -> crate::hit::HitId {
        let index = self.automatic_hit_index;
        self.automatic_hit_index = self.automatic_hit_index.saturating_add(1);
        crate::hit::HitId::new(format!("auto.{kind}.{index}"))
    }

    /// Return the active focus scope requested by this frame.
    #[must_use]
    pub const fn focus_scope(&self) -> Option<&FocusScopeId> {
        self.focus_scope.as_ref()
    }

    /// Select the focus scope active after this frame commits.
    pub fn set_focus_scope(&mut self, scope: Option<FocusScopeId>) {
        self.focus_scope = scope;
    }

    /// Return image lifecycle contributions registered for this frame.
    #[must_use]
    pub fn images(&self) -> &[ImageContribution] {
        &self.images
    }

    /// Add an image lifecycle contribution to this frame.
    pub fn push_image(&mut self, contribution: ImageContribution) {
        self.images.push(contribution);
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

    /// Add one semantic region.
    pub fn push_semantic(&mut self, region: SemanticRegion) {
        self.semantics.push(region);
    }

    /// Add one terminal-space damage region.
    pub fn push_damage(&mut self, area: Rect) {
        if !area.is_empty() {
            self.damage.push(area);
        }
    }

    /// Return bounded damage requested by rendered components.
    #[must_use]
    pub fn damage(&self, policy: DamagePolicy) -> Damage {
        Damage::regions(self.damage.iter().copied(), self.area(), policy)
    }

    /// Add or replace one hierarchical selection scope.
    pub fn push_selection_scope(&mut self, scope: SelectionScope) {
        self.selection.push_scope(scope);
    }

    /// Add one visible logical selection fragment.
    pub fn push_selection_fragment(&mut self, fragment: SelectionFragment) {
        self.selection.push_fragment(fragment);
    }

    /// Paint one logical selection snapshot over content already rendered.
    ///
    /// Calling this after component rendering gives selection a deterministic
    /// overlay stage while preserving every underlying cell symbol and
    /// semantic style field not replaced by `style`.
    pub fn paint_selection(&mut self, snapshot: &SelectionSnapshot, style: Style) {
        paint_selection_highlights(self.buffer, &snapshot.visible_highlights, style);
    }

    /// Fill a rectangular area with a symbol and style.
    pub fn fill(&mut self, area: Rect, symbol: &str, style: Style) {
        self.buffer.fill(area, symbol, style);
    }

    /// Write a styled line into a rectangular area.
    pub fn write_line(&mut self, area: Rect, line: &Line) {
        self.buffer.write_line(area, line);
    }

    /// Fill a rectangular area with `style`, then write a line whose spans
    /// inherit that fallback style.
    pub fn write_line_with_fallback_style(&mut self, area: Rect, line: &Line, style: Style) {
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
