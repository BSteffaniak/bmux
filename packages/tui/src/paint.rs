//! Scoped local-coordinate painting for composable components.

use crate::frame::{Cursor, Frame};
use crate::geometry::{Point, Rect};
use crate::hit::HitRegion;
use crate::image::ImageContribution;
use crate::selection::{SelectionFragment, SelectionScope};
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

    /// Effective terminal-space clip.
    #[must_use]
    pub const fn clip(&self) -> Rect {
        self.clip
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

    /// Paint one symbol at a local coordinate after translation and clipping.
    pub fn set_cell(&mut self, x: i32, y: i64, symbol: &str, style: Style) {
        let Some(area) = self.project_rect(LocalRect::new(x, y, 1, 1)) else {
            return;
        };
        self.frame.buffer_mut().set_cell(
            Point::new(area.x, area.y),
            symbol,
            self.inherited_style.patch(style),
        );
    }

    /// Fill a local rectangle after translation and clipping.
    pub fn fill(&mut self, area: LocalRect, symbol: &str, style: Style) {
        let Some(area) = self.project_rect(area) else {
            return;
        };
        self.frame
            .fill(area, symbol, self.inherited_style.patch(style));
    }

    /// Write a styled line after translation and clipping.
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
        let line = line.viewport(left_clip, usize::from(projected.width));
        self.frame
            .write_line_with_fallback_style(projected, &line, self.inherited_style);
    }

    /// Fill a local row and write a line whose spans inherit the supplied style.
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
        let line = line.viewport(left_clip, usize::from(projected.width));
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
        let x = i64::from(self.origin_x).saturating_add(i64::from(x));
        let y = self.origin_y.saturating_add(i64::from(y));
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
    pub fn push_semantic(&mut self, mut region: SemanticRegion) {
        let Some(area) = self.project_rect(LocalRect::terminal(region.area)) else {
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
    /// The callback supplies local coordinates and writes through scoped
    /// [`PaintCx::set_cell`], preserving translation and clipping.
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
    use crate::frame::Frame;
    use crate::geometry::{Point, Rect};
    use crate::hit::HitRegion;
    use crate::image::{ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement};
    use crate::selection::{SelectionFragment, SelectionScope};
    use crate::semantic::SemanticRegion;
    use crate::style::{Color, Style};
    use crate::text::Line;

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
        assert!(frame.hits().regions().is_empty());
        assert!(
            frame
                .damage(crate::damage::DamagePolicy::default())
                .is_none()
        );
        assert!(frame.selection().scopes().is_empty());
        assert!(frame.semantics().regions().is_empty());
        assert!(frame.images().is_empty());
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
}
