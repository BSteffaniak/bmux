//! Frame render context.

use crate::buffer::Buffer;
use crate::damage::{Damage, DamagePolicy};
use crate::focus::FocusScopeId;
use crate::geometry::{Point, Rect};
use crate::hit::{HitMap, HitRegion};
use crate::image::ImageContribution;
use crate::selection::{
    SelectionFragment, SelectionScene, SelectionScope, paint_selection_highlights,
};
use crate::semantic::{SemanticRegion, SemanticScene};
use crate::style::Style;
use crate::text::Line;

/// Regional producer declaration; absence means ordinary complete metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceMetadataAvailability {
    /// Visible terminal geometry, projected by `PaintCx`.
    pub area: Rect,
    /// Whether logical selection metadata is supplied for this region.
    pub selection: bool,
    /// Whether semantic metadata is supplied for this region.
    pub semantics: bool,
}

/// Cursor requested by a rendered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
    occlusions: Vec<(usize, Rect)>,
    selection: SelectionScene,
    semantics: SemanticScene,
    damage: Vec<Rect>,
    metadata_availability: Vec<SurfaceMetadataAvailability>,
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
            occlusions: Vec::new(),
            selection: SelectionScene::new(),
            semantics: SemanticScene::new(),
            damage: Vec::new(),
            metadata_availability: Vec::new(),
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

    /// Regional producer metadata declarations in paint order.
    #[must_use]
    pub fn metadata_availability(&self) -> &[SurfaceMetadataAvailability] {
        &self.metadata_availability
    }

    pub(crate) fn declare_metadata_availability(
        &mut self,
        declaration: SurfaceMetadataAvailability,
    ) {
        if !declaration.area.is_empty() {
            self.metadata_availability.push(declaration);
        }
    }

    /// Staged opaque rectangles with their image paint-order boundary.
    #[must_use]
    pub fn occlusions(&self) -> &[(usize, Rect)] {
        &self.occlusions
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

    /// Subtract an already projected opaque rectangle from earlier metadata.
    /// Image occlusion is resolved by the retained image scene, not by deleting
    /// source contributions. Cells are cleared with complete wide-cell repair.
    pub(crate) fn occlude(&mut self, area: Rect) {
        let area = area.intersection(self.area());
        if area.is_empty() {
            return;
        }
        self.occlusions.push((self.images.len(), area));
        self.metadata_availability = self
            .metadata_availability
            .iter()
            .flat_map(|declaration| {
                crate::image_scene::subtract_rect(declaration.area, area)
                    .into_iter()
                    .map(|visible| SurfaceMetadataAvailability {
                        area: visible,
                        ..*declaration
                    })
            })
            .collect();
        self.buffer.fill(area, " ", Style::new());
        self.cursor = self.cursor.filter(|cursor| !area.contains(cursor.position));
        let mut hits = HitMap::new();
        for hit in self.hits.regions() {
            for visible in crate::image_scene::subtract_rect(hit.area, area) {
                let mut fragment = hit.clone();
                fragment.area = visible;
                hits.push(fragment);
            }
        }
        self.hits = hits;
        let mut selection = SelectionScene::new();
        for scope in self.selection.scopes() {
            let mut scope = scope.clone();
            if !scope.initiation_area.intersection(area).is_empty() {
                // A scope has one rectangular initiation area. Conservatively
                // disable initiation rather than invent a competing shape model.
                scope.initiation_area = Rect::new(0, 0, 0, 0);
            }
            selection.push_scope(scope);
        }
        for fragment in self.selection.fragments() {
            if fragment.area.intersection(area).is_empty() {
                selection.push_fragment(fragment.clone());
            }
        }
        self.selection = selection;
        let mut semantics = SemanticScene::new();
        for region in self.semantics.regions() {
            for visible in crate::image_scene::subtract_rect(region.area, area) {
                let mut fragment = region.clone();
                fragment.area = visible;
                semantics.push(fragment);
            }
        }
        self.semantics = semantics;
    }

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
        if scope.is_some() && scope != self.focus_scope {
            // Selection also obeys scope exclusion. Previously painted scopes
            // remain logical metadata but cannot initiate or autoscroll behind
            // the foreground. Newly painted foreground scopes remain eligible.
            let mut selection = SelectionScene::new();
            for prior in self.selection.scopes() {
                let mut prior = prior.clone();
                prior.capture = crate::selection::SelectionCapture::Disabled;
                prior.auto_scroll = crate::selection::SelectionAutoScrollPolicy::disabled();
                selection.push_scope(prior);
            }
            for fragment in self.selection.fragments() {
                selection.push_fragment(fragment.clone());
            }
            self.selection = selection;
        }
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

    pub(crate) fn paint_selection(&mut self, highlights: &[Rect], style: Style) {
        paint_selection_highlights(self.buffer, highlights, style);
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
    fn occlusion_and_scope_exclusion_share_staged_metadata() {
        use crate::hit::HitRegion;
        use crate::selection::{SelectionCapture, SelectionScope};
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut frame = Frame::new(&mut buffer);
        frame.push_hit(HitRegion::new("background", Rect::new(0, 0, 8, 4)).focusable(true));
        frame.push_selection_scope(SelectionScope::new(
            "background-selection",
            Rect::new(0, 0, 8, 4),
        ));
        frame.set_cursor(Cursor::visible(Point::new(3, 1)));
        frame.occlude(Rect::new(3, 1, 2, 2));
        assert!(frame.cursor().is_none());
        assert!(frame.hits().hit_test(Point::new(3, 1)).is_none());
        assert!(frame.hits().hit_test(Point::new(0, 0)).is_some());
        frame.set_focus_scope(Some("modal".into()));
        frame.push_selection_scope(SelectionScope::new(
            "foreground-selection",
            Rect::new(3, 1, 2, 2),
        ));
        assert_eq!(
            frame.selection().scopes()[0].capture,
            SelectionCapture::Disabled
        );
        assert_eq!(
            frame.selection().scopes()[1].capture,
            SelectionCapture::Capture
        );
        assert_eq!(frame.occlusions(), &[(0, Rect::new(3, 1, 2, 2))]);
    }

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
