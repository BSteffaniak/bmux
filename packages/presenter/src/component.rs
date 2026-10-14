//! Slide composition delegates all measurement, wrapping, paint, and event geometry to BMUX.
use crate::custom::{CustomRegistry, RichContent};
use crate::model::{Block, BlockKind, Slide, TextFormat};
use crate::theme::PresentationTheme;
use bmux_markdown::{MarkdownRenderOptions, render_markdown};
use bmux_tui::component::LogicalInsets;
use bmux_tui::prelude::{
    Column, Component, ComponentRevision, Constraints, Element, Event, EventCx, EventOutcome,
    Keyed, LayoutCx, LayoutNode, Padding, PaintCx, Row, SizeBox, TextBlock, TextWrap,
};
use std::hash::{Hash, Hasher};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HiddenContent {
    #[default]
    ReserveSpace,
    Collapse,
}

pub struct SlideComponent<'a> {
    slide: &'a Slide,
    reveal_step: u32,
    theme: &'a PresentationTheme,
    custom: &'a CustomRegistry<'a>,
    rich: Option<&'a dyn RichContent>,
    hidden: HiddenContent,
    focused_block: Option<&'a crate::model::BlockId>,
}
impl<'a> SlideComponent<'a> {
    pub fn new(
        slide: &'a Slide,
        reveal_step: u32,
        theme: &'a PresentationTheme,
        custom: &'a CustomRegistry<'a>,
    ) -> Self {
        Self {
            slide,
            reveal_step,
            theme,
            custom,
            rich: None,
            hidden: HiddenContent::ReserveSpace,
            focused_block: None,
        }
    }
    /// Route keyboard/paste only to this caller-selected custom block. None routes neither.
    pub fn focused_block(mut self, id: &'a crate::model::BlockId) -> Self {
        self.focused_block = Some(id);
        self
    }
    pub fn hidden_content(mut self, hidden: HiddenContent) -> Self {
        self.hidden = hidden;
        self
    }
    pub fn rich_content(mut self, rich: &'a dyn RichContent) -> Self {
        self.rich = Some(rich);
        self
    }
    fn blocks(&self, blocks: &'a [Block], id: &str) -> Element<'_> {
        let mut column = Column::new().id(id.to_owned()).gap(self.theme.block_gap);
        for block in blocks {
            column = column.child(self.block(block));
        }
        Element::new(column)
    }
    fn block(&self, block: &'a Block) -> Element<'_> {
        let id = format!(
            "presentation/{}/{}",
            self.slide.id.as_str(),
            block.id.as_str()
        );
        let child = match &block.kind {
            BlockKind::Columns { columns } => {
                let children = columns
                    .iter()
                    .map(|column| {
                        let column_key = column.blocks.first().map_or_else(
                            || format!("empty-{}", column.weight),
                            |block| block.id.as_str().to_owned(),
                        );
                        (
                            column.weight,
                            self.blocks(&column.blocks, &format!("{id}/column/{column_key}")),
                        )
                    })
                    .collect();
                Element::new(ResponsiveColumns {
                    id: format!("{id}/columns"),
                    children,
                    column_gap: self.theme.column_gap,
                    block_gap: self.theme.block_gap,
                })
            }
            BlockKind::Custom { .. } => self.custom.build(block).unwrap_or_else(|error| {
                Element::new(
                    TextBlock::new(format!("[component unavailable: {error}]"))
                        .id(format!("{id}/diagnostic"))
                        .style(self.theme.diagnostic),
                )
            }),
            BlockKind::Text {
                content,
                format: TextFormat::Plain,
            } => Element::new(
                TextBlock::new(content.clone())
                    .id(format!("{id}/text"))
                    .style(self.theme.text),
            ),
            BlockKind::Code { .. }
            | BlockKind::Text {
                format: TextFormat::Markdown,
                ..
            } => self
                .rich
                .and_then(|rich| rich.build(block))
                .unwrap_or_else(|| {
                    Element::new(MarkdownBlock {
                        id: format!("{id}/markdown"),
                        source: block_markdown(&block.kind),
                        theme: self.theme,
                    })
                }),
            BlockKind::Image { rows, .. } | BlockKind::Mermaid { rows, .. } => {
                let provided = self.rich.and_then(|rich| rich.build(block));
                let child = provided.unwrap_or_else(|| {
                    Element::new(MarkdownBlock {
                        id: format!("{id}/fallback"),
                        source: block_markdown(&block.kind),
                        theme: self.theme,
                    })
                });
                // Minimum, not maximum: text fallback and diagnostics are never silently truncated.
                Element::new(
                    SizeBox::new(child)
                        .id(format!("{id}/rich"))
                        .min_height(u64::from(*rows)),
                )
            }
            _ => Element::new(MarkdownBlock {
                id: format!("{id}/markdown"),
                source: block_markdown(&block.kind),
                theme: self.theme,
            }),
        };
        Element::new(RevealComponent {
            child: Element::new(Keyed::new(id, child)),
            visible: block.reveal_step <= self.reveal_step,
            keyboard_target: self.focused_block == Some(&block.id),
            is_columns: matches!(&block.kind, BlockKind::Columns { .. }),
            reserve: self.hidden == HiddenContent::ReserveSpace,
        })
    }
    fn tree(&self) -> Element<'_> {
        Element::new(ReadableSlide {
            id: format!("presentation/{}/margin", self.slide.id.as_str()),
            margin: self.theme.margin,
            child: self.blocks(
                &self.slide.blocks,
                &format!("presentation/{}/body", self.slide.id.as_str()),
            ),
        })
    }
}
impl Component for SlideComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        self.tree()
            .revision()
            .combine(self.custom.revision())
            .combine(
                self.rich
                    .map_or(ComponentRevision::default(), RichContent::revision),
            )
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.tree().layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.tree().paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.tree().event(event, layout, cx)
    }
}

// Presentation policy stays above the domain-neutral composition primitives.
const MIN_COLUMN_WIDTH: u64 = 32;
const MAX_READABLE_WIDTH: u64 = 120;

struct Borrowed<'a>(&'a Element<'a>);
impl Component for Borrowed<'_> {
    fn revision(&self) -> ComponentRevision {
        self.0.revision()
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.0.layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.0.paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.0.event(event, layout, cx)
    }
}

struct ResponsiveColumns<'a> {
    id: String,
    children: Vec<(u16, Element<'a>)>,
    column_gap: u64,
    block_gap: u64,
}
impl ResponsiveColumns<'_> {
    fn stacks(&self, width: u64) -> bool {
        let gaps = self.column_gap.saturating_mul(
            u64::try_from(self.children.len().saturating_sub(1)).expect("child count fits u64"),
        );
        let available = width.saturating_sub(gaps);
        let total: u128 = self
            .children
            .iter()
            .map(|(weight, _)| u128::from(*weight))
            .sum();
        let mut seen = 0u128;
        let mut assigned = 0u64;
        self.children.iter().any(|(weight, _)| {
            seen += u128::from(*weight);
            // Match Row's cumulative integer allocation, including its remainder policy.
            let cumulative = u64::try_from(u128::from(available) * seen / total.max(1))
                .expect("column share is bounded by available width");
            let share = cumulative.saturating_sub(assigned);
            assigned = cumulative;
            share < MIN_COLUMN_WIDTH
        })
    }
    fn tree(&self, stacked: bool) -> Element<'_> {
        if stacked {
            let mut column = Column::new()
                .id(format!("{}/stack", self.id))
                .gap(self.block_gap);
            for (_, child) in &self.children {
                column = column.child(Borrowed(child));
            }
            Element::new(column)
        } else {
            let mut row = Row::new().id(self.id.clone()).gap(self.column_gap);
            for (weight, child) in &self.children {
                row = row.flex_child(*weight, Borrowed(child));
            }
            Element::new(row)
        }
    }
    fn committed_stack(&self, layout: &LayoutNode) -> bool {
        layout.id.as_str() == format!("{}/stack", self.id)
    }
}
impl Component for ResponsiveColumns<'_> {
    fn revision(&self) -> ComponentRevision {
        self.tree(false)
            .revision()
            .combine(self.tree(true).revision())
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.tree(self.stacks(constraints.max_width()))
            .layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.tree(self.committed_stack(layout)).paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.tree(self.committed_stack(layout))
            .event(event, layout, cx)
    }
}

struct ReadableSlide<'a> {
    id: String,
    margin: u64,
    child: Element<'a>,
}
impl ReadableSlide<'_> {
    fn tree(&self, width: u64) -> Padding<'_> {
        let margin = self.margin.min(width.saturating_sub(1) / 2);
        let inner = width.saturating_sub(margin.saturating_mul(2));
        let extra = inner.saturating_sub(MAX_READABLE_WIDTH);
        Padding::new(
            LogicalInsets::new(
                self.margin,
                margin + extra - extra / 2,
                self.margin,
                margin + extra / 2,
            ),
            Borrowed(&self.child),
        )
        .id(self.id.clone())
    }
}
impl Component for ReadableSlide<'_> {
    fn revision(&self) -> ComponentRevision {
        self.tree(0).revision()
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.tree(constraints.max_width()).layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.tree(layout.size.width).paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.tree(layout.size.width).event(event, layout, cx)
    }
}

/// Hidden content measures through the same component but emits no painting or input.
struct RevealComponent<'a> {
    child: Element<'a>,
    visible: bool,
    reserve: bool,
    keyboard_target: bool,
    is_columns: bool,
}
impl Component for RevealComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        self.child.revision().combine(ComponentRevision::new(
            if self.reserve {
                0
            } else {
                u64::from(self.visible)
            },
            u64::from(self.visible),
        ))
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        if self.visible || self.reserve {
            self.child.layout(constraints, cx)
        } else {
            let mut node = self.child.layout(
                Constraints::new(constraints.max_width(), constraints.max_width(), 0, Some(0)),
                cx,
            );
            node.size.height = 0;
            node.children.clear();
            node
        }
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if self.visible {
            self.child.paint(layout, cx);
        }
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        if !self.visible
            || (matches!(event, Event::Key(_) | Event::Paste(_))
                && !self.keyboard_target
                && !self.is_columns)
        {
            EventOutcome::Ignored
        } else {
            self.child.event(event, layout, cx)
        }
    }
}

struct MarkdownBlock<'a> {
    id: String,
    source: String,
    theme: &'a PresentationTheme,
}
impl MarkdownBlock<'_> {
    fn rendered(&self, width: u64) -> bmux_markdown::MarkdownRenderResult {
        let options = MarkdownRenderOptions::new(u16::try_from(width).unwrap_or(u16::MAX))
            .with_theme(self.theme.markdown)
            .with_document_id(self.id.clone());
        render_markdown(&self.source, &options)
    }
    fn text(&self, width: u64) -> TextBlock {
        let rendered = self.rendered(width);
        TextBlock::new(bmux_tui::prelude::Text::from_lines(rendered.lines))
            .id(self.id.clone())
            .wrap(TextWrap::None)
    }
}
impl Component for MarkdownBlock<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.source.hash(&mut hasher);
        self.id.hash(&mut hasher);
        let layout = hasher.finish();
        // MarkdownTheme owns all projection styles; Debug is used only for disposable cache invalidation.
        format!("{:?}", self.theme.markdown).hash(&mut hasher);
        ComponentRevision::new(layout, hasher.finish())
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.text(constraints.max_width()).layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        use bmux_tui::geometry::Rect;
        use bmux_tui::selection::{SelectionFragment, SelectionScope};
        use bmux_tui::semantic::SemanticRegion;

        let rendered = self.rendered(layout.size.width);
        TextBlock::new(bmux_tui::prelude::Text::from_lines(rendered.lines.clone()))
            .id(self.id.clone())
            .wrap(TextWrap::None)
            .paint(layout, cx);
        let scope = format!("{}/source", self.id);
        let bounds = Rect::new(
            0,
            0,
            u16::try_from(layout.size.width).unwrap_or(u16::MAX),
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        );
        let revision = self.revision().layout;
        cx.push_selection_scope(SelectionScope::new(scope.clone(), bounds).revision(revision));
        // These rectangles are the Markdown renderer's projection of the very
        // lines laid out above, not a second measurement or reconstructed raster.
        for (order, unit) in rendered.selection_provenance().iter().enumerate() {
            // Discontiguous transformations cannot truthfully be represented by
            // SelectionFragment's single source range. Do not invent one.
            let [range] = unit.source_ranges.as_slice() else {
                continue;
            };
            for (rect_index, rect) in unit.rects.iter().enumerate() {
                let area = Rect::new(rect.x, rect.y, rect.width, rect.height);
                if u64::from(rect.y) >= layout.size.height {
                    continue;
                }
                cx.push_selection_fragment(
                    SelectionFragment::new(
                        scope.clone(),
                        self.id.clone(),
                        area,
                        u64::try_from(order).expect("provenance count fits u64"),
                        range.clone(),
                    )
                    .id(format!(
                        "{}/source/{}:{}/{rect_index}",
                        self.id, range.start, range.end
                    ))
                    .revision(revision),
                );
                // Roles come from parser provenance, including its actual
                // container stack, rather than guessed text or pixel OCR.
                cx.push_semantic(SemanticRegion::new(
                    format!(
                        "{}/source/{}:{}/{rect_index}",
                        self.id, range.start, range.end
                    ),
                    area,
                    format!("markdown/{:?}/{:?}", unit.kind, unit.containers),
                ));
            }
        }
    }
    fn event(&self, _event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        EventOutcome::Ignored
    }
}

/// Canonical source strings for the shared Markdown fallback's stable content IDs.
/// Hosts attach only entries referenced by the committed selection scene using
/// `PreparedPresentation::set_selection_text`; source export remains host-owned.
/// Code uses the same fenced Markdown source consumed by its renderer, so its
/// provenance offsets are exact (never offsets guessed from highlighted cells).
pub fn markdown_selection_sources(
    slide: &Slide,
) -> std::collections::BTreeMap<bmux_tui::selection::SelectionContentId, String> {
    fn collect(
        slide: &Slide,
        blocks: &[Block],
        result: &mut std::collections::BTreeMap<bmux_tui::selection::SelectionContentId, String>,
    ) {
        for block in blocks {
            match &block.kind {
                BlockKind::Columns { columns } => {
                    for column in columns {
                        collect(slide, &column.blocks, result);
                    }
                }
                BlockKind::Custom { .. }
                | BlockKind::Text {
                    format: TextFormat::Plain,
                    ..
                } => {}
                kind => {
                    let suffix =
                        if matches!(kind, BlockKind::Image { .. } | BlockKind::Mermaid { .. }) {
                            "fallback"
                        } else {
                            "markdown"
                        };
                    result.insert(
                        format!(
                            "presentation/{}/{}/{suffix}",
                            slide.id.as_str(),
                            block.id.as_str()
                        )
                        .into(),
                        block_markdown(kind),
                    );
                }
            }
        }
    }
    let mut result = std::collections::BTreeMap::new();
    collect(slide, &slide.blocks, &mut result);
    result
}

fn fence(source: &str, language: &str) -> String {
    let longest = source
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let delimiter = "`".repeat(longest.max(2) + 1);
    format!("{delimiter}{language}\n{source}\n{delimiter}")
}
fn table_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', "<br>")
}
fn block_markdown(kind: &BlockKind) -> String {
    match kind {
        BlockKind::Text { content, .. } => content.clone(),
        BlockKind::Code { language, source } => fence(source, language.as_deref().unwrap_or("")),
        BlockKind::Mermaid { source, .. } => fence(source, "mermaid"),
        // Never echo local destination paths into a diagnostic fallback.
        BlockKind::Image { alt, .. } => {
            format!("[image: {}]", if alt.is_empty() { "image" } else { alt })
        }
        BlockKind::Table { headers, rows } => {
            let width = headers
                .len()
                .max(rows.iter().map(Vec::len).max().unwrap_or(0));
            if width == 0 {
                return String::new();
            }
            let row = |cells: &[String]| {
                format!(
                    "| {} |\n",
                    (0..width)
                        .map(|index| cells
                            .get(index)
                            .map_or(String::new(), |cell| table_cell(cell)))
                        .collect::<Vec<_>>()
                        .join(" | ")
                )
            };
            let mut markdown = row(headers);
            markdown.push_str(&format!("| {} |\n", vec!["---"; width].join(" | ")));
            for cells in rows {
                markdown.push_str(&row(cells));
            }
            markdown
        }
        BlockKind::Columns { .. } | BlockKind::Custom { .. } => String::new(),
    }
}

/// Ready-to-paint authorized image bytes; no decoder, file lookup, or network IO.
/// Reuse this for rasterized Mermaid output as well as authored images.
pub struct ImageComponent {
    id: String,
    payload: bmux_tui::image::ImagePayload,
    rows: u16,
    fit: bmux_tui::image::ImageFit,
}
impl ImageComponent {
    /// `rows` preserves authored allocation. Zero opts into intrinsic-aspect sizing,
    /// bounded to 24 logical rows; final pixel fitting uses the backend's cell metrics.
    pub fn new(id: impl Into<String>, payload: bmux_tui::image::ImagePayload, rows: u16) -> Self {
        Self {
            id: id.into(),
            payload,
            rows,
            fit: bmux_tui::image::ImageFit::Contain,
        }
    }

    fn dimensions(&self) -> (u32, u32) {
        match &self.payload {
            bmux_tui::image::ImagePayload::Png { width, height, .. }
            | bmux_tui::image::ImagePayload::Pixels { width, height, .. } => (*width, *height),
        }
    }
    fn allocated_rows(&self, constraints: Constraints) -> u64 {
        if self.rows != 0 {
            return u64::from(self.rows);
        }
        let (width, height) = self.dimensions();
        // Layout has no device cell metrics. Use a conservative 1:2 cell aspect
        // estimate for allocation only; ImageFit::Contain performs exact fitting.
        let rows = (u128::from(constraints.max_width()) * u128::from(height))
            .div_ceil(u128::from(width.max(1)) * 2);
        u64::try_from(rows.min(24))
            .expect("automatic rows are bounded")
            .min(constraints.max_height().unwrap_or(24))
    }

    /// Override the default aspect-preserving fit for intentional stretching.
    #[must_use]
    pub const fn fit(mut self, fit: bmux_tui::image::ImageFit) -> Self {
        self.fit = fit;
        self
    }
}
impl Component for ImageComponent {
    fn revision(&self) -> ComponentRevision {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.id.hash(&mut hasher);
        self.rows.hash(&mut hasher);
        if self.rows == 0 {
            self.dimensions().hash(&mut hasher);
        }
        std::mem::discriminant(&self.fit).hash(&mut hasher);
        let layout = hasher.finish();
        match &self.payload {
            bmux_tui::image::ImagePayload::Png {
                bytes,
                width,
                height,
            } => {
                bytes.hash(&mut hasher);
                width.hash(&mut hasher);
                height.hash(&mut hasher);
            }
            bmux_tui::image::ImagePayload::Pixels {
                bytes,
                width,
                height,
                format,
            } => {
                bytes.hash(&mut hasher);
                width.hash(&mut hasher);
                height.hash(&mut hasher);
                format!("{format:?}").hash(&mut hasher);
            }
        }
        ComponentRevision::new(layout, hasher.finish())
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        LayoutNode::leaf(
            bmux_tui::component::LayoutId::new(self.id.clone()),
            constraints.constrain(bmux_tui::component::LogicalSize::new(
                constraints.max_width(),
                self.allocated_rows(constraints),
            )),
        )
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let area = bmux_tui::paint::LocalRect::new(
            0,
            0,
            u16::try_from(layout.size.width).unwrap_or(u16::MAX),
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        );
        let destination = bmux_tui::geometry::Rect::new(0, 0, area.width, area.height);
        if cx.project_raster_rect(area).is_some() {
            cx.push_image(bmux_tui::image::ImageContribution::Present(
                bmux_tui::image::ImagePlacement {
                    fit: self.fit,
                    allocation: None,
                    key: bmux_tui::image::ImageKey::new(self.id.clone()),
                    payload: self.payload.clone(),
                    destination,
                    clip: destination,
                    lifecycle: bmux_tui::image::ImageLifecycle::Frame,
                },
            ));
        }
    }
    fn event(&self, _event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        EventOutcome::Ignored
    }
}

/// Explicit local-only notes surface; never composed into ViewerComponent.
pub struct SpeakerNotesComponent<'a> {
    pub slide: &'a Slide,
    pub theme: &'a PresentationTheme,
}
impl SpeakerNotesComponent<'_> {
    fn text(&self) -> TextBlock {
        TextBlock::new(self.slide.notes.text.clone())
            .id(format!(
                "presentation/{}/private-notes",
                self.slide.id.as_str()
            ))
            .style(self.theme.text)
    }
}
impl Component for SpeakerNotesComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        self.text().revision()
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.text().layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.text().paint(layout, cx);
    }
    fn event(&self, _event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        EventOutcome::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::SlideBuilder;

    fn responsive(weights: &[u16]) -> ResponsiveColumns<'static> {
        ResponsiveColumns {
            id: "columns".into(),
            children: weights
                .iter()
                .enumerate()
                .map(|(index, weight)| {
                    (
                        *weight,
                        Element::new(
                            TextBlock::new(format!("column {index}")).id(format!("child-{index}")),
                        ),
                    )
                })
                .collect(),
            column_gap: 2,
            block_gap: 1,
        }
    }

    #[test]
    fn responsive_columns_use_exact_weighted_breakpoints_and_committed_branch() {
        let columns = responsive(&[1, 1]);
        let mut cx = LayoutCx::default();
        let narrow = columns.layout(Constraints::for_width(65), &mut cx);
        assert!(columns.committed_stack(&narrow));
        assert_eq!(narrow.children[0].x, 0);
        assert_eq!(narrow.children[1].x, 0);
        assert!(narrow.children[1].y > narrow.children[0].y);
        let wide = columns.layout(Constraints::for_width(66), &mut cx);
        assert!(!columns.committed_stack(&wide));
        assert_eq!(wide.children[0].node.size.width, 32);
        assert_eq!(wide.children[1].x, 34);
        assert_eq!(wide.children[1].y, 0);
        assert_eq!(narrow, columns.layout(Constraints::for_width(65), &mut cx));

        let weighted = responsive(&[1, 2]);
        assert!(weighted.stacks(97));
        let layout = weighted.layout(Constraints::for_width(98), &mut cx);
        assert!(!weighted.committed_stack(&layout));
        assert_eq!(layout.children[0].node.size.width, 32);
        assert_eq!(layout.children[1].node.size.width, 64);
        assert!(responsive(&[0, 1]).stacks(100));
        assert!(responsive(&[1, 1]).stacks(0));
    }

    #[test]
    fn nested_columns_respond_to_their_own_allocation() {
        let outer = ResponsiveColumns {
            id: "outer".into(),
            children: vec![
                (1, Element::new(responsive(&[1, 1]))),
                (1, Element::new(TextBlock::new("sibling"))),
            ],
            column_gap: 2,
            block_gap: 1,
        };
        let layout = outer.layout(Constraints::for_width(100), &mut LayoutCx::default());
        assert!(!outer.committed_stack(&layout));
        let inner = &layout.children[0].node;
        assert_eq!(inner.id.as_str(), "columns/stack");
        assert_eq!(inner.size.width, 49);
        assert!(inner.children[1].y > 0);
    }

    #[test]
    fn readable_slide_centers_wide_content_and_keeps_tiny_viewports_usable() {
        let slide = SlideBuilder::new("s").text("text", "hello").build();
        let theme = PresentationTheme::dark();
        let custom = CustomRegistry::new();
        let component = SlideComponent::new(&slide, 0, &theme, &custom);
        let mut cx = LayoutCx::default();
        let wide = component.layout(Constraints::for_width(200), &mut cx);
        assert_eq!(wide.size.width, 200);
        assert_eq!(wide.children[0].x, 40);
        assert_eq!(wide.children[0].node.size.width, MAX_READABLE_WIDTH);
        for width in 0..=3 {
            let tiny = component.layout(Constraints::for_width(width), &mut cx);
            assert_eq!(tiny.size.width, width);
            assert_eq!(tiny.children[0].x, width.saturating_sub(1) / 2);
            assert_eq!(
                tiny.children[0].node.size.width,
                if width == 0 {
                    0
                } else {
                    1 + u64::from(width == 2)
                }
            );
        }
    }

    #[test]
    fn stacked_columns_paint_and_events_receive_the_committed_child_geometry() {
        use bmux_tui::prelude::{Buffer, Frame, Rect};
        use std::cell::Cell;
        struct Probe<'a> {
            id: &'static str,
            painted: &'a Cell<Option<bmux_tui::component::LogicalSize>>,
            routed: &'a Cell<Option<bmux_tui::component::LogicalSize>>,
        }
        impl Component for Probe<'_> {
            fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
                LayoutNode::leaf(
                    self.id.into(),
                    constraints.constrain(bmux_tui::component::LogicalSize::new(
                        constraints.max_width(),
                        2,
                    )),
                )
            }
            fn paint(&self, layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {
                self.painted.set(Some(layout.size));
            }
            fn event(
                &self,
                _event: &Event,
                layout: &LayoutNode,
                _cx: &mut EventCx<'_>,
            ) -> EventOutcome {
                self.routed.set(Some(layout.size));
                EventOutcome::Ignored
            }
        }
        let first_paint = Cell::new(None);
        let second_paint = Cell::new(None);
        let first_event = Cell::new(None);
        let second_event = Cell::new(None);
        let columns = ResponsiveColumns {
            id: "probes".into(),
            children: vec![
                (
                    1,
                    Element::new(Probe {
                        id: "first",
                        painted: &first_paint,
                        routed: &first_event,
                    }),
                ),
                (
                    1,
                    Element::new(Probe {
                        id: "second",
                        painted: &second_paint,
                        routed: &second_event,
                    }),
                ),
            ],
            column_gap: 2,
            block_gap: 1,
        };
        let layout = columns.layout(Constraints::for_width(40), &mut LayoutCx::default());
        assert!(columns.committed_stack(&layout));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 8));
        let mut frame = Frame::new(&mut buffer);
        columns.paint(&layout, &mut PaintCx::new(&mut frame));
        columns.event(&Event::Tick, &layout, &mut EventCx::new(&layout));
        assert_eq!(first_paint.get(), Some(layout.children[0].node.size));
        assert_eq!(second_paint.get(), Some(layout.children[1].node.size));
        assert_eq!(first_paint.get(), first_event.get());
        assert_eq!(second_paint.get(), second_event.get());
    }

    #[test]
    fn automatic_images_use_aspect_and_available_height_without_changing_explicit_rows() {
        use bmux_tui::image::ImagePayload;
        let make = |width, height, rows| {
            ImageComponent::new(
                "image",
                ImagePayload::Png {
                    bytes: vec![],
                    width,
                    height,
                },
                rows,
            )
        };
        let mut cx = LayoutCx::default();
        assert_eq!(
            make(400, 100, 0)
                .layout(Constraints::for_width(80), &mut cx)
                .size
                .height,
            10
        );
        assert_eq!(
            make(100, 400, 0)
                .layout(Constraints::for_width(80), &mut cx)
                .size
                .height,
            24
        );
        assert_eq!(
            make(400, 100, 0)
                .layout(Constraints::new(0, 80, 0, Some(6)), &mut cx)
                .size
                .height,
            6
        );
        assert_eq!(
            make(400, 100, 3)
                .layout(Constraints::for_width(80), &mut cx)
                .size
                .height,
            3
        );
        assert_eq!(
            make(400, 100, 0)
                .layout(Constraints::for_width(0), &mut cx)
                .size
                .height,
            0
        );
        assert_ne!(
            make(400, 100, 0).revision().layout,
            make(100, 400, 0).revision().layout
        );
        assert_eq!(
            make(400, 100, 3).revision().layout,
            make(100, 400, 3).revision().layout
        );
    }

    #[test]
    fn responsive_reveals_keep_reserved_geometry_at_each_breakpoint() {
        let columns = vec![
            crate::model::Column {
                weight: 1,
                blocks: SlideBuilder::new("left").text("a", "left").build().blocks,
            },
            crate::model::Column {
                weight: 1,
                blocks: SlideBuilder::new("right")
                    .pause()
                    .text("b", "right")
                    .build()
                    .blocks,
            },
        ];
        let slide = SlideBuilder::new("s").columns("pair", columns).build();
        let theme = PresentationTheme::dark();
        let custom = CustomRegistry::new();
        let hidden = SlideComponent::new(&slide, 0, &theme, &custom);
        let visible = SlideComponent::new(&slide, 1, &theme, &custom);
        for width in [40, 68, 100] {
            let mut cx = LayoutCx::default();
            assert_eq!(
                hidden.layout(Constraints::for_width(width), &mut cx),
                visible.layout(Constraints::for_width(width), &mut cx)
            );
        }
        assert_eq!(hidden.revision().layout, visible.revision().layout);
    }

    #[test]
    fn supplied_code_provider_owns_layout_and_paint_without_markdown_wrapping() {
        use bmux_tui::prelude::{Buffer, Frame, LayoutId, LogicalSize, Rect};
        struct PreparedCode;
        impl RichContent for PreparedCode {
            fn build<'a>(&'a self, block: &'a Block) -> Option<Element<'a>> {
                if !matches!(&block.kind, BlockKind::Code { .. }) {
                    return None;
                }
                Some(Element::new(
                    TextBlock::new("prepared code\nprovider row")
                        .id(format!("code/{}", block.id.as_str())),
                ))
            }
        }
        let slide = SlideBuilder::new("s")
            .code("code", Some("rust".into()), "ORIGINAL_SOURCE".to_owned())
            .build();
        let theme = PresentationTheme::dark();
        let custom = CustomRegistry::new();
        let provider = PreparedCode;
        let component = SlideComponent::new(&slide, 0, &theme, &custom).rich_content(&provider);
        let layout = component.layout(Constraints::for_width(30), &mut LayoutCx::default());
        let code = layout
            .find(&LayoutId::new("code/code"))
            .expect("prepared provider layout");
        assert_eq!(code.size, LogicalSize::new(28, 2));
        assert!(
            layout
                .find(&LayoutId::new("presentation/s/code/markdown"))
                .is_none()
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 8));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let visible = (0..8)
            .filter_map(|row| frame.buffer().row_symbols(row))
            .collect::<String>();
        assert!(visible.contains("prepared code"));
        assert!(visible.contains("provider row"));
        assert!(!visible.contains("ORIGINAL_SOURCE"));
        struct UnavailableCode;
        impl RichContent for UnavailableCode {
            fn build<'a>(&'a self, _block: &'a Block) -> Option<Element<'a>> {
                None
            }
        }
        let unavailable = UnavailableCode;
        let fallback = SlideComponent::new(&slide, 0, &theme, &custom).rich_content(&unavailable);
        let fallback_layout = fallback.layout(Constraints::for_width(30), &mut LayoutCx::default());
        assert!(
            fallback_layout
                .find(&LayoutId::new("presentation/s/code/markdown"))
                .is_some()
        );
    }
    #[test]
    fn shared_markdown_prose_and_code_publish_source_geometry_and_clip_on_scroll() {
        use bmux_tui::damage::Damage;
        use bmux_tui::prelude::{Rect, Size};
        use bmux_tui::presentation::PresentationState as Scene;
        let slide = SlideBuilder::new("s")
            .markdown("prose", "# Heading\n\n**héllo** world")
            .code("code", Some("rust".into()), "let café = 1;\n".repeat(12))
            .build();
        let theme = PresentationTheme::dark();
        let custom = CustomRegistry::new();
        let component = SlideComponent::new(&slide, 0, &theme, &custom);
        let layout = component.layout(Constraints::for_width(40), &mut LayoutCx::default());
        let scene = Scene::new(Rect::new(0, 0, 40, 30));
        let mut prepared = scene
            .prepare(Damage::Full, |cx| component.paint(&layout, cx))
            .unwrap();
        let sources = markdown_selection_sources(&slide);
        prepared.set_selection_text(sources.clone()).unwrap();
        for id in [
            "presentation/s/prose/markdown",
            "presentation/s/code/markdown",
        ] {
            let fragments: Vec<_> = prepared
                .selection()
                .fragments()
                .iter()
                .filter(|fragment| fragment.content_id.as_str() == id)
                .collect();
            assert!(!fragments.is_empty(), "missing source geometry for {id}");
            for fragment in fragments {
                let source = &sources[&fragment.content_id];
                assert!(source.get(fragment.source_range.clone()).is_some());
                assert!(fragment.area.bottom() <= 30);
                assert!(
                    prepared
                        .semantics()
                        .regions()
                        .iter()
                        .any(|region| region.area == fragment.area)
                );
            }
        }
        assert!(
            prepared
                .semantics()
                .regions()
                .iter()
                .any(|region| region.role.contains("Heading"))
        );
        assert!(
            prepared
                .semantics()
                .regions()
                .iter()
                .any(|region| region.role.contains("Code"))
        );

        // Use the real shared viewer's scrolling composition, not manually
        // translated provenance. Completely offscreen prose must disappear.
        let deck = crate::builder::DeckBuilder::new("d")
            .slide(slide)
            .build()
            .unwrap();
        let navigation = crate::navigation::PresentationState::new(&deck);
        let viewer = crate::viewer::ViewerState::new();
        let content =
            crate::viewer::ViewerComponent::new(&deck, &navigation, &viewer, &theme, &custom);
        let viewport = Constraints::tight(Size::new(40, 5));
        let initial = content.layout(viewport, &mut LayoutCx::default());
        let scene = Scene::new(Rect::new(0, 0, 40, 5));
        let visible = scene
            .prepare(Damage::Full, |cx| content.paint(&initial, cx))
            .unwrap();
        assert!(
            visible
                .selection()
                .fragments()
                .iter()
                .any(|fragment| fragment.content_id.as_str() == "presentation/s/prose/markdown")
        );
        let mut scroll = viewer.scroll.get();
        scroll.set_vertical_offset(100);
        viewer.scroll.set(scroll);
        let scrolled = content.layout(viewport, &mut LayoutCx::default());
        let clipped = scene
            .prepare(Damage::Full, |cx| content.paint(&scrolled, cx))
            .unwrap();
        assert!(
            !clipped
                .selection()
                .fragments()
                .iter()
                .any(|fragment| fragment.content_id.as_str() == "presentation/s/prose/markdown")
        );
        assert!(!clipped.semantics().regions().iter().any(|region| {
            region
                .id
                .starts_with("presentation/s/prose/markdown/source/")
        }));
        assert!(
            clipped
                .selection()
                .fragments()
                .iter()
                .all(|fragment| fragment.area.bottom() <= 4)
        );
    }

    #[test]
    fn markdown_source_ids_survive_block_reordering_and_width_changes() {
        let first = SlideBuilder::new("s")
            .markdown("a", "**alpha**")
            .markdown("b", "beta")
            .build();
        let mut second = first.clone();
        second.blocks.reverse();
        assert_eq!(
            markdown_selection_sources(&first),
            markdown_selection_sources(&second)
        );
        let theme = PresentationTheme::dark();
        let custom = CustomRegistry::new();
        let component = SlideComponent::new(&first, 0, &theme, &custom);
        for width in [12, 40] {
            let layout = component.layout(Constraints::for_width(width), &mut LayoutCx::default());
            let scene = bmux_tui::presentation::PresentationState::new(
                bmux_tui::geometry::Rect::new(0, 0, u16::try_from(width).unwrap(), 20),
            );
            let prepared = scene
                .prepare(bmux_tui::damage::Damage::Full, |cx| {
                    component.paint(&layout, cx)
                })
                .unwrap();
            assert!(prepared.selection().fragments().iter().all(|fragment| {
                markdown_selection_sources(&first).contains_key(&fragment.content_id)
            }));
        }
    }

    #[test]
    fn markdown_rich_provider_is_selected_for_layout_and_paint_but_not_plain_text() {
        use bmux_tui::prelude::{Buffer, Frame, LayoutId, Rect};
        use std::cell::Cell;
        struct PreparedMarkdown(Cell<usize>);
        impl RichContent for PreparedMarkdown {
            fn build<'a>(&'a self, block: &'a Block) -> Option<Element<'a>> {
                assert!(matches!(
                    block.kind,
                    BlockKind::Text {
                        format: TextFormat::Markdown,
                        ..
                    }
                ));
                self.0.set(self.0.get() + 1);
                Some(Element::new(
                    TextBlock::new("prepared markdown").id("prepared/markdown"),
                ))
            }
        }
        let slide = SlideBuilder::new("s")
            .markdown("md", "ORIGINAL_MARKDOWN")
            .text("plain", "plain text")
            .build();
        let theme = PresentationTheme::dark();
        let custom = CustomRegistry::new();
        let provider = PreparedMarkdown(Cell::new(0));
        let component = SlideComponent::new(&slide, 0, &theme, &custom).rich_content(&provider);
        let layout = component.layout(Constraints::for_width(30), &mut LayoutCx::default());
        assert_eq!(provider.0.get(), 1);
        assert!(layout.find(&LayoutId::new("prepared/markdown")).is_some());
        assert!(
            layout
                .find(&LayoutId::new("presentation/s/md/markdown"))
                .is_none()
        );
        assert!(
            layout
                .find(&LayoutId::new("presentation/s/plain/text"))
                .is_some()
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 8));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let visible = (0..8)
            .filter_map(|row| frame.buffer().row_symbols(row))
            .collect::<String>();
        assert!(visible.contains("prepared markdown"));
        assert!(visible.contains("plain text"));
        assert!(!visible.contains("ORIGINAL_MARKDOWN"));
    }

    #[test]
    fn markdown_fallback_is_unchanged_when_provider_is_absent_or_declines() {
        use bmux_tui::prelude::{Buffer, Frame, LayoutId, Rect};
        struct UnavailableMarkdown;
        impl RichContent for UnavailableMarkdown {
            fn build<'a>(&'a self, _block: &'a Block) -> Option<Element<'a>> {
                None
            }
        }
        let slide = SlideBuilder::new("s")
            .markdown("md", "fallback **markdown**")
            .build();
        let theme = PresentationTheme::dark();
        let custom = CustomRegistry::new();
        let provider = UnavailableMarkdown;
        let default = SlideComponent::new(&slide, 0, &theme, &custom);
        let declined = SlideComponent::new(&slide, 0, &theme, &custom).rich_content(&provider);
        let mut cx = LayoutCx::default();
        let layout = default.layout(Constraints::for_width(30), &mut cx);
        assert_eq!(layout, declined.layout(Constraints::for_width(30), &mut cx));
        assert!(
            layout
                .find(&LayoutId::new("presentation/s/md/markdown"))
                .is_some()
        );
        let render = |component: &SlideComponent<'_>| {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 8));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            (0..8)
                .filter_map(|row| frame.buffer().row_symbols(row))
                .collect::<String>()
        };
        let visible = render(&default);
        assert!(visible.contains("fallback markdown"));
        assert_eq!(visible, render(&declined));
    }

    #[test]
    fn reserved_reveals_preserve_geometry_and_layout_revision() {
        let slide = SlideBuilder::new("s")
            .text("a", "first")
            .pause()
            .text("b", "second\nthird")
            .build();
        let theme = PresentationTheme::dark();
        let custom = CustomRegistry::new();
        let before = SlideComponent::new(&slide, 0, &theme, &custom);
        let after = SlideComponent::new(&slide, 1, &theme, &custom);
        let mut cx = LayoutCx::default();
        let first = before.layout(Constraints::for_width(30), &mut cx);
        let second = after.layout(Constraints::for_width(30), &mut cx);
        assert_eq!(first, second);
        assert_eq!(before.revision().layout, after.revision().layout);
        assert_ne!(before.revision().paint, after.revision().paint);
        let collapsed =
            SlideComponent::new(&slide, 0, &theme, &custom).hidden_content(HiddenContent::Collapse);
        assert!(
            collapsed
                .layout(Constraints::for_width(30), &mut cx)
                .size
                .height
                < first.size.height
        );
    }
    #[test]
    fn image_paint_translates_once_and_respects_clip() {
        use bmux_tui::image::{ImageContribution, ImagePayload};
        use bmux_tui::prelude::{Buffer, Frame, Rect};
        let image = ImageComponent::new(
            "image",
            ImagePayload::Png {
                bytes: vec![1, 2],
                width: 1,
                height: 1,
            },
            3,
        );
        let layout = image.layout(Constraints::for_width(5), &mut LayoutCx::default());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 6));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(
            2,
            1,
            bmux_tui::paint::LocalRect::new(0, 0, 4, 2),
            |cx| image.paint(&layout, cx),
        );
        let ImageContribution::Present(placement) = &frame.images()[0] else {
            panic!("image")
        };
        assert_eq!(placement.destination, Rect::new(2, 1, 5, 3));
        assert_eq!(placement.clip, Rect::new(2, 1, 4, 2));
        assert_eq!(placement.fit, bmux_tui::image::ImageFit::Contain);
        assert_eq!(
            placement.allocation,
            Some(bmux_tui::image::ImageBounds {
                x: 2,
                y: 1,
                width: 5,
                height: 3
            })
        );
    }
    #[test]
    fn hidden_custom_content_emits_no_hits_or_events() {
        use bmux_tui::prelude::{Buffer, Frame, Rect};
        use bmux_tui_components::button::{ButtonComponent, ButtonState};
        use std::cell::Cell;
        struct Factory {
            state: Cell<ButtonState>,
        }
        impl crate::custom::CustomComponentFactory for Factory {
            fn build<'a>(&'a self, _block: &'a Block) -> Result<Element<'a>, String> {
                Ok(Element::new(ButtonComponent::new(
                    "linked-button",
                    "Hello",
                    &self.state,
                )))
            }
        }
        let slide = SlideBuilder::new("s")
            .pause()
            .custom("b", "button", serde_json::Value::Null)
            .build();
        let theme = PresentationTheme::dark();
        let mut custom = CustomRegistry::new();
        custom
            .register(
                "button",
                Factory {
                    state: Cell::new(ButtonState::new()),
                },
            )
            .unwrap();
        let hidden = SlideComponent::new(&slide, 0, &theme, &custom);
        let mut cx = LayoutCx::default();
        let layout = hidden.layout(Constraints::for_width(30), &mut cx);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 8));
        let mut frame = Frame::new(&mut buffer);
        hidden.paint(&layout, &mut PaintCx::new(&mut frame));
        assert!(frame.hits().regions().is_empty());
        assert_eq!(
            hidden.event(&Event::Tick, &layout, &mut EventCx::new(&layout)),
            EventOutcome::Ignored
        );
        let shown = SlideComponent::new(&slide, 1, &theme, &custom);
        shown.paint(&layout, &mut PaintCx::new(&mut frame));
        assert!(!frame.hits().regions().is_empty());
    }
    #[test]
    fn paste_reaches_only_the_explicit_target() {
        use std::cell::Cell;
        use std::rc::Rc;
        struct Probe(Rc<Cell<usize>>);
        impl Component for Probe {
            fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
                LayoutNode::leaf(
                    "probe".into(),
                    constraints.constrain(bmux_tui::component::LogicalSize::new(5, 1)),
                )
            }
            fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}
            fn event(
                &self,
                event: &Event,
                _layout: &LayoutNode,
                _cx: &mut EventCx<'_>,
            ) -> EventOutcome {
                if matches!(event, Event::Paste(_)) {
                    self.0.set(self.0.get() + 1);
                    EventOutcome::Handled
                } else {
                    EventOutcome::Ignored
                }
            }
        }
        let first = Rc::new(Cell::new(0));
        let second = Rc::new(Cell::new(0));
        let mut registry = CustomRegistry::new();
        let counter = first.clone();
        registry
            .register(
                "first",
                crate::custom::OwnedFactory::new(move |_block: &Block| Ok(Probe(counter.clone()))),
            )
            .unwrap();
        let counter = second.clone();
        registry
            .register(
                "second",
                crate::custom::OwnedFactory::new(move |_block: &Block| Ok(Probe(counter.clone()))),
            )
            .unwrap();
        let slide = SlideBuilder::new("s")
            .custom("a", "first", serde_json::Value::Null)
            .custom("b", "second", serde_json::Value::Null)
            .build();
        let theme = PresentationTheme::dark();
        let id = crate::model::BlockId::new("a");
        let component = SlideComponent::new(&slide, 0, &theme, &registry).focused_block(&id);
        let layout = component.layout(Constraints::for_width(30), &mut LayoutCx::default());
        component.event(
            &Event::Paste("text".into()),
            &layout,
            &mut EventCx::new(&layout),
        );
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 0);
        let untargeted = SlideComponent::new(&slide, 0, &theme, &registry);
        assert_eq!(
            untargeted.event(
                &Event::Paste("text".into()),
                &layout,
                &mut EventCx::new(&layout)
            ),
            EventOutcome::Ignored
        );
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 0);
    }
    #[test]
    fn code_fences_cannot_terminate_on_embedded_backticks() {
        let rendered = block_markdown(&BlockKind::Code {
            language: Some("text".into()),
            source: "```\nhello".into(),
        });
        assert!(rendered.starts_with("````text\n"));
    }
}
