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
                let mut row = Row::new()
                    .id(format!("{id}/columns"))
                    .gap(self.theme.column_gap);
                for column in columns {
                    let column_key = column.blocks.first().map_or_else(
                        || format!("empty-{}", column.weight),
                        |block| block.id.as_str().to_owned(),
                    );
                    row = row.flex_child(
                        column.weight,
                        self.blocks(&column.blocks, &format!("{id}/column/{column_key}")),
                    );
                }
                Element::new(row)
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
            BlockKind::Code { .. } => {
                self.rich
                    .and_then(|rich| rich.build(block))
                    .unwrap_or_else(|| {
                        Element::new(MarkdownBlock {
                            id: format!("{id}/markdown"),
                            source: block_markdown(&block.kind),
                            theme: self.theme,
                        })
                    })
            }
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
        Element::new(
            Padding::new(
                LogicalInsets::all(self.theme.margin),
                self.blocks(
                    &self.slide.blocks,
                    &format!("presentation/{}/body", self.slide.id.as_str()),
                ),
            )
            .id(format!("presentation/{}/margin", self.slide.id.as_str())),
        )
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
    fn text(&self, width: u64) -> TextBlock {
        let options = MarkdownRenderOptions::new(u16::try_from(width).unwrap_or(u16::MAX))
            .with_theme(self.theme.markdown)
            .with_document_id(self.id.clone());
        let rendered = render_markdown(&self.source, &options);
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
        self.text(layout.size.width).paint(layout, cx);
    }
    fn event(&self, _event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        EventOutcome::Ignored
    }
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
    pub fn new(id: impl Into<String>, payload: bmux_tui::image::ImagePayload, rows: u16) -> Self {
        Self {
            id: id.into(),
            payload,
            rows,
            fit: bmux_tui::image::ImageFit::Contain,
        }
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
                u64::from(self.rows),
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
