//! Expandable section with a clickable header.
//!
//! A [`Disclosure`] shows a header row led by an expand/collapse marker and,
//! depending on its [`DisclosureMode`], a one-line summary or a body. It is
//! composed entirely from BMUX layout primitives, so measurement, wrapping,
//! clipping, and hit regions follow the same rules as every other component.
//!
//! Expansion state is caller-owned: the header registers one hit region under
//! the caller's id, and the caller flips its own state when that region is
//! activated. Only the header is interactive, and only while it is painted.

use std::borrow::Cow;

use bmux_tui::component::{
    Component, ComponentRevision, Constraints, Element, EventCx, LayoutCx, LayoutNode,
    LogicalInsets,
};
use bmux_tui::composition::{Column, HitArea, Padding, Row, TextBlock};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::hit::HitId;
use bmux_tui::paint::PaintCx;
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span, Text};
use bmux_tui::text_width::display_width;

/// How much of a disclosure is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DisclosureMode {
    /// Header and body.
    #[default]
    Expanded,
    /// Header and one-line summary.
    Summary,
    /// Header only.
    Collapsed,
}

impl DisclosureMode {
    /// Whether the marker should show the expanded glyph.
    #[must_use]
    pub const fn is_expanded(self) -> bool {
        matches!(self, Self::Expanded)
    }
}

/// Marker glyphs for the expanded and collapsed states.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DisclosureGlyphs {
    /// Marker shown while expanded.
    pub expanded: Cow<'static, str>,
    /// Marker shown while collapsed or summarized.
    pub collapsed: Cow<'static, str>,
}

impl DisclosureGlyphs {
    /// Unicode triangles (`▾`/`▸`).
    pub const UNICODE: Self = Self {
        expanded: Cow::Borrowed("▾"),
        collapsed: Cow::Borrowed("▸"),
    };
    /// ASCII markers (`v`/`>`) for terminals without Unicode fonts.
    pub const ASCII: Self = Self {
        expanded: Cow::Borrowed("v"),
        collapsed: Cow::Borrowed(">"),
    };
    /// No markers; the header alone indicates the section.
    pub const NONE: Self = Self {
        expanded: Cow::Borrowed(""),
        collapsed: Cow::Borrowed(""),
    };

    fn marker(&self, mode: DisclosureMode) -> &str {
        if mode.is_expanded() {
            &self.expanded
        } else {
            &self.collapsed
        }
    }

    /// Display width reserved for markers, so headers align across states.
    #[must_use]
    pub fn width(&self) -> usize {
        display_width(&self.expanded).max(display_width(&self.collapsed))
    }
}

impl Default for DisclosureGlyphs {
    fn default() -> Self {
        Self::UNICODE
    }
}

/// Disclosure styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisclosureStyles {
    /// Expand/collapse marker.
    pub marker: Style,
    /// One-line summary shown in [`DisclosureMode::Summary`].
    pub summary: Style,
}

impl Default for DisclosureStyles {
    fn default() -> Self {
        crate::theme::ComponentTheme::default().disclosure_styles()
    }
}

impl crate::theme::ComponentTheme {
    /// Convert into disclosure styles.
    #[must_use]
    pub fn disclosure_styles(self) -> DisclosureStyles {
        DisclosureStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for DisclosureStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            marker: theme.muted,
            summary: theme.muted,
        }
    }
}

/// Builder for a [`Disclosure`].
pub struct DisclosureBuilder<'a> {
    id: HitId,
    header: Line,
    summary: Option<Line>,
    body: Option<Element<'a>>,
    mode: DisclosureMode,
    glyphs: DisclosureGlyphs,
    styles: DisclosureStyles,
    gap: u64,
}

impl<'a> DisclosureBuilder<'a> {
    /// Set how much is shown.
    #[must_use]
    pub const fn mode(mut self, mode: DisclosureMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set the summary line shown in [`DisclosureMode::Summary`].
    #[must_use]
    pub fn summary(mut self, summary: impl Into<Line>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// Set the body shown in [`DisclosureMode::Expanded`].
    #[must_use]
    pub fn body(mut self, body: impl Component + 'a) -> Self {
        self.body = Some(Element::new(body));
        self
    }

    /// Set marker glyphs.
    #[must_use]
    pub fn glyphs(mut self, glyphs: DisclosureGlyphs) -> Self {
        self.glyphs = glyphs;
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: DisclosureStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Set the gap, in cells, between the marker and the header.
    #[must_use]
    pub const fn gap(mut self, gap: u64) -> Self {
        self.gap = gap;
        self
    }

    /// Compose the disclosure.
    ///
    /// The summary and body are indented by the marker column so content lines
    /// up under the header text. A body or summary that the mode hides is
    /// dropped rather than laid out.
    #[must_use]
    pub fn build(self) -> Disclosure<'a> {
        let marker_width = self.glyphs.width();
        let indent = if marker_width == 0 {
            0
        } else {
            u64::try_from(marker_width)
                .unwrap_or(u64::MAX)
                .saturating_add(self.gap)
        };
        let mut header = Row::new().gap(if marker_width == 0 { 0 } else { self.gap });
        if marker_width > 0 {
            // Reserve the wider marker so the header does not shift on toggle.
            let marker = self.glyphs.marker(self.mode);
            let pad = marker_width.saturating_sub(display_width(marker));
            let marker_line = Line::from_spans(vec![Span::styled(
                format!("{marker}{}", " ".repeat(pad)),
                self.styles.marker,
            )]);
            header = header.child(TextBlock::new(Text {
                lines: vec![marker_line],
            }));
        }
        header = header.flex_child(
            1,
            TextBlock::new(Text {
                lines: vec![self.header],
            }),
        );
        let mut column = Column::new().child(HitArea::new(self.id.clone(), header));
        let insets = LogicalInsets {
            left: indent,
            ..LogicalInsets::default()
        };
        match (self.mode, self.summary, self.body) {
            (DisclosureMode::Summary, Some(summary), _) => {
                // Summary style is the base; explicit span styles still win.
                let summary = Line::from_spans(
                    summary
                        .spans
                        .into_iter()
                        .map(|span| {
                            let style = self.styles.summary.patch(span.style);
                            Span::styled(span.content, style)
                        })
                        .collect::<Vec<_>>(),
                );
                column = column.child(Padding::new(
                    insets,
                    TextBlock::new(Text {
                        lines: vec![summary],
                    }),
                ));
            }
            (DisclosureMode::Expanded, _, Some(body)) => {
                column = column.child(Padding::new(insets, body));
            }
            _ => {}
        }
        Disclosure {
            id: self.id,
            mode: self.mode,
            inner: column,
        }
    }
}

/// Composed expandable section. Create one with [`Disclosure::builder`].
pub struct Disclosure<'a> {
    id: HitId,
    mode: DisclosureMode,
    inner: Column<'a>,
}

impl<'a> Disclosure<'a> {
    /// Start a disclosure whose header activates `id`.
    #[must_use]
    pub fn builder(id: impl Into<HitId>, header: impl Into<Line>) -> DisclosureBuilder<'a> {
        DisclosureBuilder {
            id: id.into(),
            header: header.into(),
            summary: None,
            body: None,
            mode: DisclosureMode::default(),
            glyphs: DisclosureGlyphs::default(),
            styles: DisclosureStyles::default(),
            gap: 1,
        }
    }

    /// Hit id registered by the header.
    #[must_use]
    pub const fn id(&self) -> &HitId {
        &self.id
    }

    /// Mode this disclosure was built with.
    #[must_use]
    pub const fn mode(&self) -> DisclosureMode {
        self.mode
    }
}

impl Component for Disclosure<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.inner.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.inner.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.inner.event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        self.inner.revision()
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::*;

    fn render(component: &Disclosure<'_>, width: u16) -> (Vec<String>, Frame<'static>) {
        let layout = component.layout(
            Constraints::for_width(u64::from(width)),
            &mut LayoutCx::new(),
        );
        let height = u16::try_from(layout.size.height).unwrap();
        let buffer = Box::leak(Box::new(Buffer::empty(Rect::new(0, 0, width, height))));
        let mut frame = Frame::new(buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| frame.buffer().get(Point::new(x, y)).unwrap().symbol.clone())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        (rows, frame)
    }

    #[test]
    fn modes_show_header_summary_or_body() {
        let build = |mode| {
            Disclosure::builder("tool.1", "read src/main.rs")
                .summary("120 lines")
                .body(TextBlock::new("fn main() {}"))
                .mode(mode)
                .build()
        };
        assert_eq!(
            render(&build(DisclosureMode::Expanded), 24).0,
            ["▾ read src/main.rs", "  fn main() {}"]
        );
        assert_eq!(
            render(&build(DisclosureMode::Summary), 24).0,
            ["▸ read src/main.rs", "  120 lines"]
        );
        assert_eq!(
            render(&build(DisclosureMode::Collapsed), 24).0,
            ["▸ read src/main.rs"]
        );
    }

    #[test]
    fn header_alone_registers_the_hit_region() {
        let disclosure = Disclosure::builder("tool.1", "header")
            .body(TextBlock::new("body"))
            .build();
        let (_, frame) = render(&disclosure, 12);
        let hits = frame.hits().regions();
        let region = hits
            .iter()
            .find(|region| region.id == HitId::from("tool.1"))
            .expect("header hit region");
        assert_eq!(region.area, Rect::new(0, 0, 12, 1));
        assert_eq!(
            hits.iter()
                .filter(|region| region.id == HitId::from("tool.1"))
                .count(),
            1
        );
    }

    #[test]
    fn wide_markers_and_tiny_widths_stay_contained() {
        let glyphs = DisclosureGlyphs {
            expanded: Cow::Borrowed("🔽"),
            collapsed: Cow::Borrowed(">"),
        };
        let collapsed = Disclosure::builder("x", "界面 header")
            .glyphs(glyphs.clone())
            .mode(DisclosureMode::Collapsed)
            .build();
        let expanded = Disclosure::builder("x", "界面 header")
            .glyphs(glyphs)
            .body(TextBlock::new("body"))
            .build();
        // The narrower marker is padded, so header text starts in the same column.
        let collapsed_rows = render(&collapsed, 16).0;
        let expanded_rows = render(&expanded, 16).0;
        assert!(
            collapsed_rows[0].starts_with(">  界面"),
            "{collapsed_rows:?}"
        );
        assert!(expanded_rows[0].starts_with("🔽 界面"), "{expanded_rows:?}");
        for width in [0, 1, 2, 3] {
            let layout = expanded.layout(Constraints::for_width(width), &mut LayoutCx::new());
            assert!(layout.size.width <= width);
        }
    }

    #[test]
    fn no_marker_glyphs_render_header_flush_left() {
        let disclosure = Disclosure::builder("x", "plain")
            .glyphs(DisclosureGlyphs::NONE)
            .summary("sum")
            .mode(DisclosureMode::Summary)
            .build();
        assert_eq!(render(&disclosure, 10).0, ["plain", "sum"]);
    }
}
