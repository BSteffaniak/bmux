//! Scoped local-coordinate painting for composable components.
//!
//! # Painting model
//!
//! * [`PaintCx::fill`] sets the style of every cell in an area. It is how a
//!   background is painted, and how one is cleared (fill with an explicit
//!   [`Color::Default`](crate::style::Color) background).
//! * [`PaintCx::write_line`] and [`PaintCx::set_cell`] draw *over* what is
//!   already there: colors a style leaves unset (`None`) keep the cell's
//!   existing values, so foreground-only text keeps the background beneath
//!   it, and cells past the end of the text are untouched.
//! * [`PaintCx::write_line_with_fallback_style`] fills its area first, then
//!   writes; [`PaintCx::replace_cell`] replaces one cell's whole style.

use crate::component::{Component, Constraints, LayoutCx};
use crate::focus::FocusScopeId;
use crate::frame::{Cursor, Frame};
use crate::geometry::{Point, Rect};
use crate::hit::HitRegion;
use crate::image::ImageContribution;
use crate::selection::{SelectionFragment, SelectionScope, SelectionSnapshot, SelectionTextRun};
use crate::semantic::SemanticRegion;
use crate::style::Style;
use crate::text::Line;

/// Signed local-coordinate rectangle used before clipping to terminal space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalRect {
    /// Local x coordinate.
    pub x: i32,
    /// Local y coordinate.
    pub y: i64,
    /// Width in terminal cells.
    pub width: u16,
    /// Height in terminal rows.
    pub height: u16,
}

impl LocalRect {
    /// Create a local rectangle.
    #[must_use]
    pub const fn new(x: i32, y: i64, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Create a local rectangle from terminal-space geometry.
    #[must_use]
    pub const fn terminal(rect: Rect) -> Self {
        Self::new(rect.x as i32, rect.y as i64, rect.width, rect.height)
    }
}

/// Mutable paint context carrying the effective origin, clip, and inherited style.
pub struct PaintCx<'frame, 'buffer> {
    frame: &'frame mut Frame<'buffer>,
    origin_x: i32,
    origin_y: i64,
    clip: Rect,
    inherited_style: Style,
}

impl<'frame, 'buffer> PaintCx<'frame, 'buffer> {
    /// Create a root paint context over the complete frame.
    pub const fn new(frame: &'frame mut Frame<'buffer>) -> Self {
        let clip = frame.area();
        Self {
            frame,
            origin_x: 0,
            origin_y: 0,
            clip,
            inherited_style: Style::new(),
        }
    }

    /// The presentation clock of the frame being painted, when its driver
    /// supplied one.
    ///
    /// Every context of one frame returns the same value, so time-derived
    /// cells, hit regions, and selection fragments agree. Components must read
    /// this instead of a wall clock.
    #[must_use]
    pub const fn clock(&self) -> Option<crate::clock::PresentationClock> {
        self.frame.clock()
    }

    /// A fresh layout context carrying this frame's presentation clock.
    ///
    /// Use it to resolve layout inside a render callback so layout and paint
    /// observe the same instant.
    #[must_use]
    pub const fn layout_cx(&self) -> LayoutCx {
        match self.frame.clock() {
            Some(clock) => LayoutCx::with_clock(clock),
            None => LayoutCx::new(),
        }
    }

    /// Effective terminal-space clip.
    #[must_use]
    pub const fn clip(&self) -> Rect {
        self.clip
    }

    /// The local rectangle covering this context's effective clip.
    ///
    /// For a root context this is the complete frame area; nested contexts
    /// see the region their parent assigned to them, expressed in their own
    /// local coordinates.
    #[must_use]
    pub fn area(&self) -> LocalRect {
        let x = i64::from(self.clip.x)
            .saturating_sub(i64::from(self.origin_x))
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX));
        let y = i64::from(self.clip.y).saturating_sub(self.origin_y);
        LocalRect::new(
            i32::try_from(x).unwrap_or(i32::MAX),
            y,
            self.clip.width,
            self.clip.height,
        )
    }

    /// The cursor requested so far in this frame, in terminal coordinates.
    #[must_use]
    pub const fn cursor(&self) -> Option<Cursor> {
        self.frame.cursor()
    }

    /// Return the visible cursor in this context's local logical coordinates.
    ///
    /// Read this after painting a child to anchor geometry in the enclosing
    /// scope. The inverse uses the actual scoped origin, not the clipped edge,
    /// so sliced documents retain their source row (including rows beyond
    /// `u16::MAX`). No cursor, a hidden cursor, or a cursor outside this scope's
    /// effective clip returns `None`. As with [`Self::set_cursor_logical`], the
    /// right insertion edge is included and the bottom edge is excluded.
    /// Negative or unrepresentable local coordinates also return `None`.
    ///
    /// This reads the current frame request, not a committed presentation or
    /// proof that the cursor belongs to a particular child. Callers retain
    /// ownership and commit geometry only after successful presentation.
    #[must_use]
    pub fn logical_cursor(&self) -> Option<(u64, u64)> {
        let cursor = self.frame.cursor()?;
        let position = cursor.position;
        if !cursor.visible
            || self.clip.is_empty()
            || position.x < self.clip.x
            || position.x > self.clip.right()
            || position.y < self.clip.y
            || position.y >= self.clip.bottom()
        {
            return None;
        }
        let x = i64::from(position.x).checked_sub(i64::from(self.origin_x))?;
        let y = i64::from(position.y).checked_sub(self.origin_y)?;
        Some((u64::try_from(x).ok()?, u64::try_from(y).ok()?))
    }

    /// Paint one logical selection snapshot over content already rendered.
    ///
    /// This is the deterministic overlay stage a root presenter callback runs
    /// after composing its tree; highlights are already terminal-space
    /// rectangles produced from committed selection geometry, so they are
    /// intersected with the effective clip rather than translated.
    pub fn paint_selection(&mut self, snapshot: &SelectionSnapshot, style: Style) {
        self.paint_selection_highlights(&snapshot.visible_highlights, style);
    }

    /// Paint terminal-space selection highlight rectangles over content
    /// already rendered, clipped to the effective clip.
    ///
    /// Use it for highlights derived from a selection scene by other means
    /// than a [`SelectionSnapshot`], for example logical selections spanning
    /// virtualized items whose endpoints are not in the scene.
    pub fn paint_selection_highlights(&mut self, highlights: &[Rect], style: Style) {
        for highlight in highlights {
            let clipped = highlight.intersection(self.clip);
            if !clipped.is_empty() {
                self.frame
                    .paint_selection(std::slice::from_ref(&clipped), style);
            }
        }
    }

    /// Current inherited style.
    #[must_use]
    pub const fn inherited_style(&self) -> Style {
        self.inherited_style
    }

    /// Read-only view of the selection metadata registered so far in this frame.
    #[must_use]
    pub const fn selection(&self) -> &crate::selection::SelectionScene {
        self.frame.selection()
    }

    /// Paint in a translated child coordinate system with an additional local clip.
    pub fn with_child(
        &mut self,
        offset_x: i32,
        offset_y: i64,
        local_clip: LocalRect,
        paint: impl FnOnce(&mut PaintCx<'_, 'buffer>),
    ) {
        let child_origin_x = self.origin_x.saturating_add(offset_x);
        let child_origin_y = self.origin_y.saturating_add(offset_y);
        let requested_clip = translate_and_clip(
            child_origin_x,
            child_origin_y,
            local_clip,
            self.frame.area(),
        );
        let clip = self.clip.intersection(requested_clip);
        if clip.is_empty() {
            return;
        }
        let mut child = PaintCx {
            frame: self.frame,
            origin_x: child_origin_x,
            origin_y: child_origin_y,
            clip,
            inherited_style: self.inherited_style,
        };
        paint(&mut child);
    }

    /// Paint a child with logical dimensions, clipping before terminal conversion.
    /// Unlike a terminal-sized clip, this preserves visible rows deep inside a
    /// document taller than `u16::MAX`.
    pub fn with_child_size(
        &mut self,
        offset_x: i32,
        offset_y: i64,
        size: crate::component::LogicalSize,
        paint: impl FnOnce(&mut PaintCx<'_, 'buffer>),
    ) {
        let area = self.area();
        let left = area.x.saturating_sub(offset_x).max(0);
        let right = area
            .x
            .saturating_sub(offset_x)
            .saturating_add(i32::from(area.width))
            .min(i32::try_from(size.width).unwrap_or(i32::MAX));
        let width = u16::try_from(right.saturating_sub(left)).unwrap_or(0);
        let top = area.y.saturating_sub(offset_y).max(0);
        let bottom = area
            .y
            .saturating_sub(offset_y)
            .saturating_add(i64::from(area.height))
            .min(i64::try_from(size.height).unwrap_or(i64::MAX));
        let height = u16::try_from(bottom.saturating_sub(top)).unwrap_or(0);
        self.with_child(
            offset_x,
            offset_y,
            LocalRect::new(left, top, width, height),
            paint,
        );
    }

    /// Lay out `component` to exactly fill `area` and paint it there.
    ///
    /// `area` is in this context's local coordinates; the component paints in
    /// its own local coordinates and is clipped to `area`. The layout is
    /// computed fresh on every call, so components that need retained layout
    /// caching should go through a [`crate::component::LayoutCache`] and
    /// [`Self::with_child`] instead.
    pub fn paint_component(&mut self, component: &(impl Component + ?Sized), area: Rect) {
        if area.is_empty() {
            return;
        }
        let layout = component.layout(Constraints::tight(area.size()), &mut self.layout_cx());
        self.with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| component.paint(&layout, cx),
        );
    }

    /// Paint with an additional inherited style.
    pub fn with_style(&mut self, style: Style, paint: impl FnOnce(&mut PaintCx<'_, 'buffer>)) {
        let mut child = PaintCx {
            frame: self.frame,
            origin_x: self.origin_x,
            origin_y: self.origin_y,
            clip: self.clip,
            inherited_style: self.inherited_style.patch(style),
        };
        paint(&mut child);
    }

    /// Paint one symbol at a local coordinate after translation and clipping,
    /// layering `style` over the cell beneath: colors `style` leaves unset
    /// keep the existing cell's (so a foreground-only border keeps the
    /// background under it). Use an explicit color, such as
    /// [`Color::Default`](crate::style::Color), to change the background, or
    /// [`Self::replace_cell`] to replace the whole style.
    pub fn set_cell(&mut self, x: i32, y: i64, symbol: &str, style: Style) {
        let Some(area) = self.project_rect(LocalRect::new(x, y, 1, 1)) else {
            return;
        };
        self.frame.buffer_mut().overlay_cell(
            Point::new(area.x, area.y),
            symbol,
            self.inherited_style.patch(style),
        );
    }

    /// Paint one symbol at a local coordinate, replacing the cell's whole
    /// style (unset colors become the terminal default).
    pub fn replace_cell(&mut self, x: i32, y: i64, symbol: &str, style: Style) {
        let Some(area) = self.project_rect(LocalRect::new(x, y, 1, 1)) else {
            return;
        };
        self.frame.buffer_mut().set_cell(
            Point::new(area.x, area.y),
            symbol,
            self.inherited_style.patch(style),
        );
    }

    /// Layer `style` over the cells of a local rectangle after translation and
    /// clipping, keeping their symbols. Colors and modifiers `style` leaves unset
    /// keep each cell's own, so already painted chrome (a border, a title) can be
    /// highlighted in place without knowing what was drawn there.
    pub fn restyle(&mut self, area: LocalRect, style: Style) {
        let Some(area) = self.project_rect(area) else {
            return;
        };
        let buffer = self.frame.buffer_mut();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if let Some(cell) = buffer.get_mut(Point::new(x, y)) {
                    cell.style = cell.style.patch(style);
                }
            }
        }
    }

    /// Fill a local rectangle after translation and clipping, replacing each
    /// cell's style. Filling is how a background is set or cleared.
    pub fn fill(&mut self, area: LocalRect, symbol: &str, style: Style) {
        let Some(area) = self.project_rect(area) else {
            return;
        };
        self.frame
            .fill(area, symbol, self.inherited_style.patch(style));
    }

    /// Write a styled line after translation and clipping, layering each
    /// span over the cells beneath: colors a span leaves unset keep the
    /// existing background/foreground, and cells past the end of the text are
    /// untouched. To clear or set the row's background first, use
    /// [`Self::write_line_with_fallback_style`] (which fills `area`) or
    /// [`Self::fill`].
    pub fn write_line(&mut self, area: LocalRect, line: &Line) {
        let Some(projected) = self.project_rect(area) else {
            return;
        };
        let left_clip = usize::try_from(
            i64::from(projected.x)
                .saturating_sub(projected_unclipped_x(self.origin_x, area.x))
                .max(0),
        )
        .unwrap_or(usize::MAX);
        let line = visible_line(line, left_clip, usize::from(projected.width));
        self.frame
            .buffer_mut()
            .overlay_line_with_fallback(projected, &line, self.inherited_style);
    }

    /// Fill `area` with the supplied style (replacing what was there), then
    /// write a line whose spans inherit it.
    pub fn write_line_with_fallback_style(&mut self, area: LocalRect, line: &Line, style: Style) {
        let Some(projected) = self.project_rect(area) else {
            return;
        };
        let left_clip = usize::try_from(
            i64::from(projected.x)
                .saturating_sub(projected_unclipped_x(self.origin_x, area.x))
                .max(0),
        )
        .unwrap_or(usize::MAX);
        let line = visible_line(line, left_clip, usize::from(projected.width));
        self.frame.write_line_with_fallback_style(
            projected,
            &line,
            self.inherited_style.patch(style),
        );
    }

    /// Request a cursor at a local point when it lies inside the effective clip.
    pub fn set_cursor(&mut self, position: Point, visible: bool) {
        let Some(position) = self.project_point(position) else {
            return;
        };
        self.frame.set_cursor(if visible {
            Cursor::visible(position)
        } else {
            Cursor::hidden(position)
        });
    }

    /// Request a cursor at a local coordinate. A cursor at the right edge is
    /// allowed because terminal cursors may occupy the insertion position just
    /// beyond the final painted cell.
    pub fn set_cursor_local(&mut self, x: u16, y: u16, visible: bool) {
        self.set_cursor_logical(u64::from(x), u64::from(y), visible);
    }

    /// Request a logical cursor, clipping before conversion to terminal coordinates.
    pub fn set_cursor_logical(&mut self, x: u64, y: u64, visible: bool) {
        let Ok(x) = i64::try_from(x) else { return };
        let Ok(y) = i64::try_from(y) else { return };
        let x = i64::from(self.origin_x).saturating_add(x);
        let y = self.origin_y.saturating_add(y);
        if x < i64::from(self.clip.x)
            || x > i64::from(self.clip.right())
            || y < i64::from(self.clip.y)
            || y >= i64::from(self.clip.bottom())
        {
            return;
        }
        let position = Point::new(
            u16::try_from(x).unwrap_or(u16::MAX),
            u16::try_from(y).unwrap_or(u16::MAX),
        );
        self.frame.set_cursor(if visible {
            Cursor::visible(position)
        } else {
            Cursor::hidden(position)
        });
    }

    /// Select the focus scope active after this frame commits.
    ///
    /// Focusable hit regions registered afterwards without an explicit scope
    /// inherit it, so a modal paints its scope before painting its controls.
    pub fn set_focus_scope(&mut self, scope: Option<FocusScopeId>) {
        self.frame.set_focus_scope(scope);
    }

    /// Return the focus scope requested so far in this frame.
    #[must_use]
    pub const fn focus_scope(&self) -> Option<&FocusScopeId> {
        self.frame.focus_scope()
    }

    /// Declare whether a producer supplies logical metadata for a local region.
    /// This is an availability statement, never metadata reconstructed from cells.
    pub fn declare_metadata_availability(
        &mut self,
        area: LocalRect,
        selection: bool,
        semantics: bool,
    ) {
        if let Some(area) = self.project_rect(area) {
            self.frame
                .declare_metadata_availability(crate::frame::SurfaceMetadataAvailability {
                    area,
                    selection,
                    semantics,
                });
        }
    }

    /// Occlude earlier staged output using this context's translation and clip.
    /// Source image lifetime remains owned by the retained scene and caller.
    pub fn occlude(&mut self, area: LocalRect) {
        if let Some(area) = self.project_rect(area) {
            self.frame.occlude(area);
        }
    }

    /// Register local damage after translation and clipping.
    pub fn push_damage(&mut self, area: LocalRect) {
        if let Some(area) = self.project_rect(area) {
            self.frame.push_damage(area);
        }
    }

    /// Register a local interaction region after translation and clipping.
    pub fn push_hit(&mut self, mut region: HitRegion) {
        let Some(area) = self.project_rect(LocalRect::terminal(region.area)) else {
            return;
        };
        region.area = area;
        self.frame.push_hit(region);
    }

    /// Register local focus geometry without enabling pointer handling.
    pub fn push_focus(&mut self, id: impl Into<crate::hit::HitId>, area: LocalRect) {
        let Some(area) = self.project_rect(area) else {
            return;
        };
        self.frame.push_hit(
            HitRegion::new(id, area)
                .pointer_events(false)
                .focusable(true),
        );
    }

    /// Register a local semantic region after translation and clipping.
    pub fn push_semantic(&mut self, region: SemanticRegion) {
        let area = LocalRect::terminal(region.area);
        self.push_semantic_in(area, region);
    }

    /// Register semantic metadata for a local rectangle with a logical row offset.
    pub fn push_semantic_in(&mut self, area: LocalRect, mut region: SemanticRegion) {
        let Some(area) = self.project_rect(area) else {
            return;
        };
        region.area = area;
        self.frame.push_semantic(region);
    }

    /// Register a local selection scope after translating and clipping both areas.
    pub fn push_selection_scope(&mut self, mut scope: SelectionScope) {
        let Some(area) = self.project_rect(LocalRect::terminal(scope.area)) else {
            return;
        };
        let initiation_area = self
            .project_rect(LocalRect::terminal(scope.initiation_area))
            .unwrap_or_else(|| Rect::new(area.x, area.y, 0, 0));
        scope.area = area;
        scope.initiation_area = initiation_area;
        self.frame.push_selection_scope(scope);
    }

    /// Register a local selection fragment after translation and clipping.
    /// Partially clipped fragments are omitted so their complete logical unit is
    /// never mapped to incomplete terminal geometry.
    pub fn push_selection_fragment(&mut self, mut fragment: SelectionFragment) {
        let local = LocalRect::terminal(fragment.area);
        let translated = translate_and_clip(self.origin_x, self.origin_y, local, self.frame.area());
        if translated.width != local.width || translated.height != local.height {
            return;
        }
        let Some(area) = self.project_rect(local) else {
            return;
        };
        if area != translated {
            return;
        }
        fragment.area = area;
        self.frame.push_selection_fragment(fragment);
    }

    /// Register a local text run after translation and clipping. Its units
    /// are materialized as fragments only when the frame's selection scene is
    /// queried, and, like [`Self::push_selection_fragment`], units partially
    /// outside this context's clip are omitted. A run whose row is clipped
    /// away registers nothing.
    pub fn push_selection_text_run(&mut self, run: SelectionTextRun) {
        if let Some(run) = run.translate_and_clip(self.origin_x, self.origin_y, self.clip) {
            self.frame.push_selection_text_run(run);
        }
    }

    /// Register an image contribution, translating its destination and
    /// intersecting its explicit clip with the effective component clip.
    pub fn push_image(&mut self, contribution: ImageContribution) {
        let contribution = match contribution {
            ImageContribution::Present(mut placement) => {
                let destination = translate_and_clip(
                    self.origin_x,
                    self.origin_y,
                    LocalRect::terminal(placement.destination),
                    self.frame.area(),
                );
                if destination.is_empty() {
                    return;
                }
                let Some(clip) = self.project_rect(LocalRect::terminal(placement.clip)) else {
                    return;
                };
                if placement.fit == crate::image::ImageFit::Contain {
                    let local = placement
                        .allocation
                        .unwrap_or_else(|| placement.destination.into());
                    placement.allocation = Some(crate::image::ImageBounds {
                        x: self.origin_x.saturating_add(local.x),
                        y: i32::try_from(self.origin_y.saturating_add(i64::from(local.y)))
                            .unwrap_or(if self.origin_y < 0 {
                                i32::MIN
                            } else {
                                i32::MAX
                            }),
                        width: local.width,
                        height: local.height,
                    });
                }
                placement.destination = destination;
                placement.clip = clip;
                ImageContribution::Present(placement)
            }
            ImageContribution::Remove(key) => ImageContribution::Remove(key),
        };
        self.frame.push_image(contribution);
    }

    /// Project a local rectangle into the effective terminal-space clip.
    ///
    /// This is the bounded geometry bridge for low-level raster producers.
    /// Callers receive only the visible terminal rectangle and never mutable
    /// access to the backing frame or buffer.
    #[must_use]
    pub fn project_raster_rect(&self, area: LocalRect) -> Option<Rect> {
        self.project_rect(area)
    }

    /// Visit each visible terminal cell corresponding to a local raster area.
    ///
    /// The callback receives coordinates relative to `area` (its top-left is
    /// `(0, 0)`) and writes through scoped [`PaintCx::set_cell`], preserving
    /// translation and clipping. Returning `None` leaves that cell unchanged.
    pub fn rasterize(
        &mut self,
        area: LocalRect,
        mut cell: impl FnMut(i32, i64) -> Option<(String, Style)>,
    ) {
        let Some(visible) = self.project_rect(area) else {
            return;
        };
        let translated_x = i64::from(self.origin_x).saturating_add(i64::from(area.x));
        let translated_y = self.origin_y.saturating_add(area.y);
        for terminal_y in visible.y..visible.bottom() {
            for terminal_x in visible.x..visible.right() {
                let local_x = i64::from(terminal_x).saturating_sub(translated_x);
                let local_y = i64::from(terminal_y).saturating_sub(translated_y);
                let Ok(local_x) = i32::try_from(local_x) else {
                    continue;
                };
                if let Some((symbol, style)) = cell(local_x, local_y) {
                    self.set_cell(
                        area.x.saturating_add(local_x),
                        area.y.saturating_add(local_y),
                        &symbol,
                        style,
                    );
                }
            }
        }
    }

    fn project_rect(&self, area: LocalRect) -> Option<Rect> {
        let projected = translate_and_clip(self.origin_x, self.origin_y, area, self.clip);
        (!projected.is_empty()).then_some(projected)
    }

    fn project_point(&self, point: Point) -> Option<Point> {
        let x = self.origin_x.saturating_add(i32::from(point.x));
        let y = self.origin_y.saturating_add(i64::from(point.y));
        let x = u16::try_from(x).ok()?;
        let y = u16::try_from(y).ok()?;
        let point = Point::new(x, y);
        self.clip.contains(point).then_some(point)
    }
}

/// The part of `line` visible `left_clip` cells in and `width` cells wide,
/// borrowed when that is the whole line (the common case: no copy).
fn visible_line(line: &Line, left_clip: usize, width: usize) -> std::borrow::Cow<'_, Line> {
    if left_clip == 0 && width > 0 && crate::text::line_fits(line, width) {
        std::borrow::Cow::Borrowed(line)
    } else {
        std::borrow::Cow::Owned(line.viewport(left_clip, width))
    }
}

fn projected_unclipped_x(origin_x: i32, local_x: i32) -> i64 {
    i64::from(origin_x.saturating_add(local_x))
}

fn translate_and_clip(origin_x: i32, origin_y: i64, area: LocalRect, clip: Rect) -> Rect {
    let left = i64::from(origin_x.saturating_add(area.x));
    let top = origin_y.saturating_add(area.y);
    let right = left.saturating_add(i64::from(area.width));
    let bottom = top.saturating_add(i64::from(area.height));
    let clip_left = i64::from(clip.x);
    let clip_top = i64::from(clip.y);
    let clip_right = i64::from(clip.right());
    let clip_bottom = i64::from(clip.bottom());
    let left = left.max(clip_left);
    let top = top.max(clip_top);
    let right = right.min(clip_right);
    let bottom = bottom.min(clip_bottom);
    if right <= left || bottom <= top {
        return Rect::new(clip.x, clip.y, 0, 0);
    }
    Rect::new(
        u16::try_from(left).unwrap_or(clip.x),
        u16::try_from(top).unwrap_or(clip.y),
        u16::try_from(right.saturating_sub(left)).unwrap_or(u16::MAX),
        u16::try_from(bottom.saturating_sub(top)).unwrap_or(u16::MAX),
    )
}

#[cfg(test)]
mod tests {
    use super::{LocalRect, PaintCx};
    use crate::buffer::Buffer;
    use crate::component::{Component, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize};
    use crate::frame::Frame;
    use crate::geometry::{Point, Rect};
    use crate::hit::HitRegion;
    use crate::image::{ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement};
    use crate::selection::{SelectionFragment, SelectionScope};
    use crate::semantic::SemanticRegion;
    use crate::style::{Color, Modifier, Style};
    use crate::text::Line;

    fn filled(width: u16) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
        buffer.fill(
            Rect::new(0, 0, width, 1),
            " ",
            Style::new().fg(Color::White).bg(Color::Blue),
        );
        buffer
    }

    fn style_at(buffer: &Buffer, x: u16) -> Style {
        buffer.get(Point::new(x, 0)).unwrap().style
    }

    #[test]
    fn text_keeps_the_background_beneath_it() {
        let mut buffer = filled(6);
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);
        paint.write_line(
            LocalRect::new(0, 0, 6, 1),
            &Line::from_spans(vec![crate::text::Span::styled(
                "ab",
                Style::new().fg(Color::Red),
            )]),
        );
        paint.set_cell(4, 0, "│", Style::new().fg(Color::Green));
        assert_eq!(
            style_at(&buffer, 0),
            Style::new().fg(Color::Red).bg(Color::Blue)
        );
        assert_eq!(
            style_at(&buffer, 2),
            Style::new().fg(Color::White).bg(Color::Blue),
            "cells past the text are untouched"
        );
        assert_eq!(
            style_at(&buffer, 4),
            Style::new().fg(Color::Green).bg(Color::Blue)
        );
        assert_eq!(buffer.get(Point::new(1, 0)).unwrap().symbol, "b");
    }

    #[test]
    fn restyle_keeps_symbols_and_unset_colors_and_clips() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        buffer.fill(
            Rect::new(0, 0, 4, 2),
            "│",
            Style::new().fg(Color::Blue).bg(Color::Black),
        );
        {
            let mut frame = Frame::new(&mut buffer);
            let mut paint = PaintCx::new(&mut frame);
            paint.with_child(1, 0, LocalRect::new(0, 0, 2, 2), |paint| {
                // Wider than the clip: only the two clipped columns change.
                paint.restyle(
                    LocalRect::new(0, 0, 5, 1),
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                );
            });
        }
        for x in 0..4 {
            let cell = buffer.get(Point::new(x, 0)).unwrap();
            assert_eq!(cell.symbol, "│");
            assert_eq!(cell.style.bg, Some(Color::Black));
            let highlighted = (1..3).contains(&x);
            assert_eq!(cell.style.fg == Some(Color::Cyan), highlighted, "x = {x}");
            assert_eq!(cell.style.modifiers.contains(Modifier::BOLD), highlighted);
        }
        assert_eq!(
            buffer.get(Point::new(1, 1)).unwrap().style.fg,
            Some(Color::Blue)
        );
    }

    #[test]
    fn backgrounds_can_still_be_changed_or_cleared() {
        let mut buffer = filled(6);
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);
        // An explicit color (including Default) replaces the background.
        paint.write_line(
            LocalRect::new(0, 0, 2, 1),
            &Line::from_spans(vec![crate::text::Span::styled(
                "ab",
                Style::new().bg(Color::Default),
            )]),
        );
        // replace_cell replaces the whole style.
        paint.replace_cell(2, 0, "x", Style::new().fg(Color::Red));
        // write_line_with_fallback_style fills its area first.
        paint.write_line_with_fallback_style(
            LocalRect::new(3, 0, 3, 1),
            &Line::raw("z"),
            Style::new(),
        );
        assert_eq!(style_at(&buffer, 0).bg, Some(Color::Default));
        assert_eq!(style_at(&buffer, 2), Style::new().fg(Color::Red));
        assert_eq!(style_at(&buffer, 3), Style::new());
        assert_eq!(style_at(&buffer, 5), Style::new());
    }

    #[test]
    fn selection_overlay_clips_terminal_coordinates_without_mutating_snapshot() {
        use crate::selection::{SelectionAffinity, SelectionEndpoint, SelectionSnapshot};
        let endpoint = SelectionEndpoint {
            scope_id: "scope".into(),
            content_id: "content".into(),
            offset: 0,
            order: 0,
            affinity: SelectionAffinity::Before,
            revision: 0,
        };
        let snapshot = SelectionSnapshot {
            scope_id: "scope".into(),
            anchor: endpoint.clone(),
            focus: endpoint,
            reversed: false,
            slices: Vec::new(),
            visible_highlights: vec![Rect::new(0, 1, 8, 1), Rect::new(0, 0, 2, 1)],
        };
        let original = snapshot.clone();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
        let content_style = Style::new()
            .fg(Color::Blue)
            .add_modifier(crate::style::Modifier::BOLD);
        buffer.fill(Rect::new(0, 0, 8, 3), "x", content_style);
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(2, 1, LocalRect::new(1, 0, 3, 1), |paint| {
            paint.paint_selection(&snapshot, Style::new().bg(Color::Red));
        });
        for y in 0..3 {
            assert_eq!(frame.buffer().row_symbols(y).as_deref(), Some("xxxxxxxx"));
            for x in 0..8 {
                let expected = if y == 1 && (3..6).contains(&x) {
                    content_style.bg(Color::Red)
                } else {
                    content_style
                };
                assert_eq!(
                    frame.buffer().get(Point::new(x, y)).unwrap().style,
                    expected
                );
            }
        }
        assert_eq!(snapshot, original);
    }

    #[test]
    fn focus_scope_set_through_context_is_inherited_by_later_hits() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
        let mut frame = Frame::new(&mut buffer);
        {
            let mut paint = PaintCx::new(&mut frame);
            paint.push_hit(HitRegion::new("background", Rect::new(0, 0, 8, 1)).focusable(true));
            assert!(paint.focus_scope().is_none());
            paint.set_focus_scope(Some(crate::hit::HitId::new("modal")));
            paint.with_child(2, 1, LocalRect::new(0, 0, 4, 1), |paint| {
                paint.push_hit(HitRegion::new("modal.ok", Rect::new(0, 0, 4, 1)).focusable(true));
            });
            assert_eq!(
                paint.focus_scope().map(crate::hit::HitId::as_str),
                Some("modal")
            );
        }

        let regions = frame.hits().regions();
        assert_eq!(regions.len(), 2);
        assert!(regions[0].focus_scope.is_none());
        assert_eq!(
            regions[1]
                .focus_scope
                .as_ref()
                .map(crate::hit::HitId::as_str),
            Some("modal")
        );
        assert_eq!(regions[1].area, Rect::new(2, 1, 4, 1));
        assert_eq!(
            frame.focus_scope().map(crate::hit::HitId::as_str),
            Some("modal")
        );
    }

    #[test]
    fn logical_cursor_round_trips_nested_sliced_source_coordinates() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 6));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);
        assert_eq!(paint.logical_cursor(), None);
        paint.with_child(3, 1, LocalRect::new(0, 0, 6, 4), |owner| {
            owner.with_child(-2, -70_000, LocalRect::new(2, 70_000, 6, 4), |source| {
                source.with_child(4, 70_002, LocalRect::new(0, 0, 2, 1), |leaf| {
                    leaf.set_cursor_logical(1, 0, true);
                    assert_eq!(leaf.logical_cursor(), Some((1, 0)));
                });
                assert_eq!(source.logical_cursor(), Some((5, 70_002)));
                assert_eq!(
                    source.cursor().map(|cursor| cursor.position),
                    Some(Point::new(6, 3))
                );
            });
            assert_eq!(owner.logical_cursor(), Some((3, 2)));
        });
        assert_eq!(paint.logical_cursor(), Some((6, 3)));
    }

    #[test]
    fn logical_cursor_obeys_visibility_and_insertion_edge_clipping() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 4));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);
        paint.with_child(2, 1, LocalRect::new(0, 0, 3, 2), |cx| {
            cx.set_cursor_logical(3, 1, true);
            assert_eq!(cx.logical_cursor(), Some((3, 1)));
            cx.set_cursor_logical(1, 1, false);
            assert!(cx.cursor().is_some());
            assert_eq!(cx.logical_cursor(), None);
        });
        for position in [
            Point::new(1, 1),
            Point::new(6, 1),
            Point::new(2, 0),
            Point::new(2, 3),
        ] {
            paint.set_cursor(position, true);
            paint.with_child(2, 1, LocalRect::new(0, 0, 3, 2), |cx| {
                assert_eq!(cx.logical_cursor(), None);
            });
        }
    }

    #[test]
    fn logical_cursor_checked_inverse_rejects_negative_overflow_and_empty_clip() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 4));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);
        paint.set_cursor(Point::new(1, 1), true);
        // Signed local paint regions can extend left/up of their origin. They
        // remain paintable, but are not unsigned logical cursor positions.
        paint.with_child(2, 2, LocalRect::new(-2, -2, 10, 4), |cx| {
            assert_eq!(cx.logical_cursor(), None);
        });
        paint.origin_y = i64::MIN;
        assert_eq!(paint.logical_cursor(), None);
        paint.origin_y = 0;
        paint.clip.width = 0;
        assert_eq!(paint.logical_cursor(), None);
    }

    #[test]
    fn logical_cursor_retains_nonzero_frame_origin_and_tiny_right_edge() {
        let mut buffer = Buffer::empty(Rect::new(5, 7, 1, 1));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);
        paint.with_child(5, 7, LocalRect::new(0, 0, 1, 1), |cx| {
            cx.set_cursor_logical(1, 0, true);
            assert_eq!(cx.logical_cursor(), Some((1, 0)));
        });
        assert_eq!(paint.logical_cursor(), Some((6, 7)));
    }

    #[test]
    fn nested_translation_and_clipping_bound_cells() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);

        paint.with_child(-2, 1, LocalRect::new(0, 0, 6, 2), |paint| {
            paint.fill(
                LocalRect::new(0, 0, 6, 2),
                "x",
                Style::new().bg(Color::Blue),
            );
        });

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("        "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("xxxx    "));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("xxxx    "));
    }

    #[test]
    fn logical_semantics_follow_nested_clips_and_omit_offscreen_regions() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 5));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(3, 1, LocalRect::new(0, 0, 4, 3), |cx| {
            cx.with_child(0, -70_000, LocalRect::new(0, 70_000, 4, 3), |cx| {
                cx.push_semantic_in(
                    LocalRect::new(-1, 69_999, 8, 3),
                    SemanticRegion::new("visible", Rect::default(), "status"),
                );
                cx.push_semantic_in(
                    LocalRect::new(0, 70_003, 4, 1),
                    SemanticRegion::new("hidden", Rect::default(), "status"),
                );
            });
        });
        let regions = frame.semantics().regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].id, "visible");
        assert_eq!(regions[0].role, "status");
        assert_eq!(regions[0].area, Rect::new(3, 1, 4, 2));
    }

    #[test]
    fn line_is_viewported_at_left_clip() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);

        paint.with_child(-2, 0, LocalRect::new(0, 0, 6, 1), |paint| {
            paint.write_line(LocalRect::new(0, 0, 6, 1), &Line::from("abcdef"));
        });

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("cdef"));
    }

    #[test]
    fn focus_geometry_translates_and_clips_without_pointer_behavior() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);

        paint.with_child(-1, 0, LocalRect::new(0, 0, 4, 1), |paint| {
            paint.push_focus("focus", LocalRect::new(0, 0, 4, 1));
            paint.push_damage(LocalRect::new(0, 0, 4, 1));
        });

        let focus = &frame.hits().regions()[0];
        assert_eq!(focus.area, Rect::new(0, 0, 3, 1));
        assert!(focus.focusable);
        assert!(!focus.pointer_events);
        assert_eq!(
            frame.damage(crate::damage::DamagePolicy {
                max_regions: 64,
                max_area_percent: 101,
            }),
            crate::damage::Damage::Regions(vec![Rect::new(0, 0, 3, 1)])
        );
    }

    #[test]
    fn nested_negative_clips_bound_cursor_hits_and_wide_text() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);

        paint.with_child(-1, 0, LocalRect::new(0, 0, 5, 1), |paint| {
            paint.with_child(0, 0, LocalRect::new(1, 0, 3, 1), |paint| {
                paint.write_line(LocalRect::new(0, 0, 5, 1), &Line::from("界abc"));
                paint.set_cursor(Point::new(0, 0), true);
                paint.push_hit(HitRegion::new("part", Rect::new(0, 0, 5, 1)));
            });
        });

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("ab "));
        assert!(frame.cursor().is_none());
        assert_eq!(frame.hits().regions()[0].area, Rect::new(0, 0, 3, 1));
    }

    #[test]
    fn completely_offscreen_child_contributes_no_metadata() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);
        let placement = ImagePlacement {
            fit: crate::image::ImageFit::Stretch,
            allocation: None,
            key: ImageKey::new("offscreen"),
            payload: ImagePayload::Png {
                bytes: vec![],
                width: 1,
                height: 1,
            },
            destination: Rect::new(0, 0, 1, 1),
            clip: Rect::new(0, 0, 1, 1),
            lifecycle: ImageLifecycle::Frame,
        };

        paint.with_child(5, 0, LocalRect::new(0, 0, 1, 1), |paint| {
            paint.set_cursor(Point::new(0, 0), true);
            paint.push_focus("offscreen-focus", LocalRect::new(0, 0, 1, 1));
            paint.push_damage(LocalRect::new(0, 0, 1, 1));
            paint.push_hit(HitRegion::new("offscreen", Rect::new(0, 0, 1, 1)));
            paint.push_selection_scope(SelectionScope::new("offscreen", Rect::new(0, 0, 1, 1)));
            paint.push_semantic(SemanticRegion::new(
                "offscreen",
                Rect::new(0, 0, 1, 1),
                "button",
            ));
            paint.push_image(ImageContribution::Present(placement));
        });

        assert!(frame.cursor().is_none());
        assert_eq!(frame.hits().regions(), []);
        assert!(
            frame
                .damage(crate::damage::DamagePolicy::default())
                .is_none()
        );
        assert_eq!(frame.selection().scopes(), []);
        assert_eq!(frame.semantics().regions(), []);
        assert_eq!(frame.images(), []);
    }

    #[test]
    fn raster_coordinates_are_relative_to_area_and_none_preserves_cells() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        let mut frame = Frame::new(&mut buffer);
        let mut visited = Vec::new();
        PaintCx::new(&mut frame).with_child(-2, 0, LocalRect::new(0, 0, 8, 2), |paint| {
            paint.rasterize(LocalRect::new(3, 1, 3, 1), |x, y| {
                visited.push((x, y));
                (x != 1).then(|| (x.to_string(), Style::new()))
            });
        });
        assert_eq!(visited, [(0, 0), (1, 0), (2, 0)]);
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("      "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some(" 0 2  "));
    }

    #[test]
    fn child_size_clips_both_axes_before_painting() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 2));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child_size(
            -40_000,
            -70_000,
            crate::component::LogicalSize::new(40_002, 70_001),
            |paint| {
                assert_eq!(paint.area(), LocalRect::new(40_000, 70_000, 2, 1));
                paint.rasterize(paint.area(), |_, _| Some(("x".into(), Style::new())));
            },
        );
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("xx "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("   "));
        PaintCx::new(&mut frame).with_child_size(
            -40_003,
            0,
            crate::component::LogicalSize::new(40_002, 1),
            |_| panic!("offscreen child painted"),
        );
    }

    #[test]
    fn bounded_rasterization_visits_only_visible_local_cells() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 2));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);
        let mut visited = Vec::new();

        paint.with_child(-1, 0, LocalRect::new(0, 0, 4, 2), |paint| {
            assert_eq!(
                paint.project_raster_rect(LocalRect::new(0, 0, 4, 2)),
                Some(Rect::new(0, 0, 3, 2))
            );
            paint.rasterize(LocalRect::new(0, 0, 4, 2), |x, y| {
                visited.push((x, y));
                Some((x.to_string(), Style::new()))
            });
        });

        assert_eq!(visited, [(1, 0), (2, 0), (3, 0), (1, 1), (2, 1), (3, 1)]);
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("123"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("123"));
    }

    #[test]
    fn selection_scope_is_clipped_but_partial_logical_fragments_are_omitted() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);

        paint.with_child(-1, 0, LocalRect::new(0, 0, 5, 2), |paint| {
            paint.push_selection_scope(SelectionScope::new("scope", Rect::new(0, 0, 5, 2)));
            paint.push_selection_fragment(SelectionFragment::new(
                "scope",
                "content",
                Rect::new(0, 0, 2, 1),
                0,
                0..2,
            ));
            paint.push_selection_fragment(SelectionFragment::new(
                "scope",
                "content",
                Rect::new(2, 0, 1, 1),
                1,
                2..3,
            ));
        });

        assert_eq!(frame.selection().scopes()[0].area, Rect::new(0, 0, 4, 2));
        assert_eq!(frame.selection().fragments().len(), 1);
        assert_eq!(frame.selection().fragments()[0].area, Rect::new(1, 0, 1, 1));
    }

    #[test]
    fn image_destination_translates_without_cropping_payload_geometry() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);

        paint.with_child(-1, 0, LocalRect::new(0, 0, 5, 2), |paint| {
            paint.push_image(ImageContribution::Present(ImagePlacement {
                fit: crate::image::ImageFit::Contain,
                allocation: None,
                key: ImageKey::new("image"),
                payload: ImagePayload::Png {
                    bytes: vec![1],
                    width: 10,
                    height: 10,
                },
                destination: Rect::new(0, 0, 5, 2),
                clip: Rect::new(0, 0, 5, 2),
                lifecycle: ImageLifecycle::Frame,
            }));
        });

        let ImageContribution::Present(placement) = &frame.images()[0] else {
            panic!("expected image placement");
        };
        assert_eq!(placement.destination, Rect::new(0, 0, 4, 2));
        assert_eq!(placement.clip, Rect::new(0, 0, 4, 2));
        assert_eq!(
            placement.allocation,
            Some(crate::image::ImageBounds {
                x: -1,
                y: 0,
                width: 5,
                height: 2
            })
        );
    }

    #[test]
    fn hit_regions_share_visual_translation_and_clip() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);

        paint.with_child(-1, 1, LocalRect::new(0, 0, 4, 1), |paint| {
            paint.push_hit(HitRegion::new("child", Rect::new(0, 0, 4, 1)));
        });

        assert_eq!(frame.hits().regions()[0].area, Rect::new(0, 1, 3, 1));
        assert!(frame.hits().regions()[0].contains(Point::new(2, 1)));
        assert!(!frame.hits().regions()[0].contains(Point::new(3, 1)));
    }

    /// Fills its whole layout with `#` and records the size it was given.
    struct Fill;

    impl Component for Fill {
        fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
            LayoutNode::leaf(
                LayoutId::new("fill"),
                constraints.constrain(LogicalSize::new(u64::MAX, u64::MAX)),
            )
        }

        fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
            let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
            for row in 0..i64::try_from(layout.size.height).unwrap_or(0) {
                cx.write_line(
                    LocalRect::new(0, row, width, 1),
                    &Line::from("#".repeat(usize::from(width))),
                );
            }
        }
    }

    #[test]
    fn paint_component_fills_exactly_the_requested_local_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 4));
        let mut frame = Frame::new(&mut buffer);
        let mut cx = PaintCx::new(&mut frame);
        cx.with_child(1, 1, LocalRect::new(0, 0, 5, 3), |cx| {
            cx.paint_component(&Fill, Rect::new(1, 0, 2, 2));
        });
        let rows: Vec<_> = (0..4)
            .map(|y| frame.buffer().row_symbols(y).unwrap_or_default())
            .collect();
        assert_eq!(rows, ["      ", "  ##  ", "  ##  ", "      "]);
    }

    #[test]
    fn paint_component_skips_empty_areas() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).paint_component(&Fill, Rect::new(0, 0, 0, 1));
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("   "));
    }
}
