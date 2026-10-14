//! Bounded preparation, performed before presentation or on a preparation worker.
use anyhow::{Context, Result, ensure};
use bmux_presenter::custom::RichContent;
use bmux_presenter::{Block, BlockKind};
use bmux_presenter_runtime::Asset;
use bmux_tui::component::Element;
use bmux_tui::image::ImagePayload;
use std::collections::BTreeMap;
use unicode_segmentation::UnicodeSegmentation;
include!("highlight.rs");

#[derive(Clone, Default)]
pub(crate) struct RichAssets {
    pub images: std::sync::Arc<BTreeMap<String, ImagePayload>>,
    pub code: std::sync::Arc<BTreeMap<String, bmux_tui::text::Text>>,
}
impl RichAssets {
    pub fn prepare(assets: &[Asset]) -> Result<Self> {
        let mut images = BTreeMap::new();
        let mut decoded_bytes = 0_usize;
        let mut encoded_bytes = 0_usize;
        ensure!(assets.len() <= 128, "asset count exceeds budget");
        for asset in assets {
            ensure!(asset.bytes.len() <= 8 * 1024 * 1024, "asset exceeds budget");
            let payload = if asset.media_type == "image/svg+xml" {
                let options = resvg::usvg::Options::default();
                let tree = resvg::usvg::Tree::from_data(&asset.bytes, &options)
                    .context("invalid diagram SVG")?;
                let size = tree.size().to_int_size();
                ensure!(
                    u64::from(size.width()) * u64::from(size.height()) <= 16 * 1024 * 1024,
                    "diagram exceeds pixel budget"
                );
                let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
                    .context("allocating diagram")?;
                resvg::render(
                    &tree,
                    resvg::tiny_skia::Transform::identity(),
                    &mut pixmap.as_mut(),
                );
                ImagePayload::Png {
                    bytes: pixmap.encode_png()?,
                    width: size.width(),
                    height: size.height(),
                }
            } else {
                let mut reader = image::ImageReader::new(std::io::Cursor::new(&asset.bytes))
                    .with_guessed_format()?;
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(4096);
                limits.max_image_height = Some(4096);
                limits.max_alloc = Some(64 * 1024 * 1024);
                reader.limits(limits);
                let decoded = reader.decode()?;
                // Keep prepared PNG bytes for compact protocol uploads. Decoding
                // here still validates the full stream under the existing limits.
                let width = decoded.width();
                let height = decoded.height();
                let bytes = if image::guess_format(&asset.bytes)? == image::ImageFormat::Png {
                    asset.bytes.clone()
                } else {
                    let mut encoded = std::io::Cursor::new(Vec::new());
                    decoded.write_to(&mut encoded, image::ImageFormat::Png)?;
                    encoded.into_inner()
                };
                ImagePayload::Png {
                    bytes,
                    width,
                    height,
                }
            };
            let size = match &payload {
                ImagePayload::Png { width, height, .. } => {
                    usize::try_from(u64::from(*width) * u64::from(*height) * 4)
                        .context("decoded asset size overflow")?
                }
                ImagePayload::Pixels { bytes, .. } => bytes.len(),
            };
            decoded_bytes = decoded_bytes
                .checked_add(size)
                .context("decoded asset size overflow")?;
            ensure!(
                decoded_bytes <= 64 * 1024 * 1024,
                "decoded asset bundle exceeds memory budget"
            );
            let prepared_bytes = match &payload {
                ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes.len(),
            };
            ensure!(
                prepared_bytes <= 8 * 1024 * 1024,
                "prepared asset exceeds budget"
            );
            encoded_bytes = encoded_bytes
                .checked_add(prepared_bytes)
                .context("prepared asset size overflow")?;
            ensure!(
                encoded_bytes <= 32 * 1024 * 1024,
                "prepared asset bundle exceeds budget"
            );
            images.insert(asset.id.clone(), payload);
        }
        Ok(Self {
            images: std::sync::Arc::new(images),
            code: std::sync::Arc::new(BTreeMap::new()),
        })
    }
}
impl RichContent for RichAssets {
    fn build<'a>(&'a self, block: &'a Block) -> Option<Element<'a>> {
        if let BlockKind::Code { source, .. } = &block.kind {
            return self.code.get(block.id.as_str()).map(|text| {
                Element::new(PreparedCode {
                    id: format!("code/{}", block.id.as_str()),
                    source,
                    text: text.clone(),
                })
            });
        }
        let BlockKind::Image {
            destination, rows, ..
        } = &block.kind
        else {
            return None;
        };
        self.images.get(destination).map(|payload| {
            Element::new(bmux_presenter::component::ImageComponent::new(
                format!("presentation/image/{}", block.id.as_str()),
                payload.clone(),
                *rows,
            ))
        })
    }
}

/// Highlighting changes style only; authored UTF-8 offsets remain authoritative.
struct PreparedCode<'a> {
    id: String,
    source: &'a str,
    text: bmux_tui::text::Text,
}
impl bmux_tui::component::Component for PreparedCode<'_> {
    fn layout(
        &self,
        constraints: bmux_tui::component::Constraints,
        cx: &mut bmux_tui::component::LayoutCx,
    ) -> bmux_tui::component::LayoutNode {
        bmux_tui::prelude::TextBlock::new(self.text.clone())
            .id(self.id.clone())
            .wrap(bmux_tui::text::TextWrap::None)
            .layout(constraints, cx)
    }
    fn paint(
        &self,
        layout: &bmux_tui::component::LayoutNode,
        cx: &mut bmux_tui::paint::PaintCx<'_, '_>,
    ) {
        use bmux_tui::geometry::Rect;
        use bmux_tui::selection::{SelectionFragment, SelectionScope};
        use bmux_tui::semantic::SemanticRegion;
        bmux_tui::prelude::TextBlock::new(self.text.clone())
            .id(self.id.clone())
            .wrap(bmux_tui::text::TextWrap::None)
            .paint(layout, cx);
        let scope = format!("{}/selection", self.id);
        cx.push_selection_scope(SelectionScope::new(
            scope.clone(),
            Rect::new(
                0,
                0,
                u16::try_from(layout.size.width).unwrap_or(u16::MAX),
                u16::try_from(layout.size.height).unwrap_or(u16::MAX),
            ),
        ));
        let mut offset = 0;
        for (row, line) in self.source.split_inclusive('\n').enumerate() {
            let mut column = 0_usize;
            for (byte, grapheme) in line.trim_end_matches(['\r', '\n']).grapheme_indices(true) {
                let width = bmux_tui::text_width::display_width(grapheme);
                if width > 0
                    && u64::try_from(row).unwrap_or(u64::MAX) < layout.size.height
                    && u64::try_from(column.saturating_add(width)).unwrap_or(u64::MAX)
                        <= layout.size.width
                {
                    let area = Rect::new(
                        u16::try_from(column).unwrap_or(u16::MAX),
                        u16::try_from(row).unwrap_or(u16::MAX),
                        u16::try_from(width).unwrap_or(u16::MAX),
                        1,
                    );
                    let start = offset + byte;
                    cx.push_selection_fragment(SelectionFragment::new(
                        scope.clone(),
                        self.id.clone(),
                        area,
                        u64::try_from(start).unwrap_or(u64::MAX),
                        start..start + grapheme.len(),
                    ));
                    cx.push_semantic(SemanticRegion::new(
                        format!("{}/source/{start}", self.id),
                        area,
                        "code/source",
                    ));
                }
                column = column.saturating_add(width);
            }
            offset += line.len();
        }
    }
}

pub(crate) fn code_selection_sources(
    slide: &bmux_presenter::Slide,
) -> BTreeMap<bmux_tui::selection::SelectionContentId, String> {
    fn collect(
        blocks: &[Block],
        output: &mut BTreeMap<bmux_tui::selection::SelectionContentId, String>,
    ) {
        for block in blocks {
            match &block.kind {
                BlockKind::Code { source, .. } => {
                    output.insert(format!("code/{}", block.id.as_str()).into(), source.clone());
                }
                BlockKind::Columns { columns } => {
                    for column in columns {
                        collect(&column.blocks, output);
                    }
                }
                _ => {}
            }
        }
    }
    let mut output = BTreeMap::new();
    collect(&slide.blocks, &mut output);
    output
}

#[cfg(test)]
mod tests {
    use bmux_tui::component::Component;

    #[test]
    fn prepared_code_provenance_uses_whole_graphemes_and_discards_clipped_cells() {
        let source = "界e\u{301}Z";
        let component = PreparedCode {
            id: "code/test".into(),
            source,
            text: bmux_tui::text::Text::from(source),
        };
        let layout = component.layout(
            Constraints::tight(bmux_tui::prelude::Size::new(4, 1)),
            &mut LayoutCx::default(),
        );
        let scene = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 4, 1));
        let prepared = scene
            .prepare(bmux_tui::damage::Damage::Full, |cx| {
                cx.with_child(
                    0,
                    0,
                    bmux_tui::prelude::LocalRect::new(0, 0, 3, 1),
                    |child| component.paint(&layout, child),
                );
            })
            .unwrap();
        let selected = prepared
            .selection()
            .fragments()
            .iter()
            .map(|fragment| &source[fragment.source_range.clone()])
            .collect::<Vec<_>>();
        assert_eq!(selected, ["界", "e\u{301}"]);
        assert!(
            prepared
                .selection()
                .fragments()
                .iter()
                .all(|fragment| fragment.area.right() <= 3)
        );
        assert!(!prepared.semantics().regions().is_empty());
    }
    use super::*;
    use bmux_tui::component::{Constraints, LayoutCx};
    use bmux_tui::{geometry::Rect, terminal::Terminal};
    #[test]
    fn prepared_png_preserves_compact_bytes_and_dimensions() {
        let bytes = include_bytes!("../../../packages/presenter/examples/architecture.png");
        let rich = RichAssets::prepare(&[Asset {
            id: "architecture".into(),
            media_type: "image/png".into(),
            bytes: bytes.to_vec(),
        }])
        .unwrap();
        assert!(matches!(
            &rich.images["architecture"],
            ImagePayload::Png { bytes: retained, width: 640, height: 240 }
                if retained.as_slice() == bytes
        ));
    }

    #[test]
    fn prepared_png_budget_counts_decoded_pixels_not_compressed_bytes() {
        let encoded = include_bytes!("../../../packages/presenter/examples/architecture.png");
        // Each small compressed image expands to 614,400 RGBA bytes.
        let assets = (0..110)
            .map(|id| Asset {
                id: id.to_string(),
                media_type: "image/png".into(),
                bytes: encoded.to_vec(),
            })
            .collect::<Vec<_>>();
        assert!(RichAssets::prepare(&assets).is_err());
    }

    #[test]
    fn rasterized_diagram_contributes_committed_image_scene() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="red"/></svg>"#;
        let rich = RichAssets::prepare(&[Asset {
            id: "diagram".into(),
            media_type: "image/svg+xml".into(),
            bytes: svg.to_vec(),
        }])
        .unwrap();
        let block = Block {
            id: "diagram-block".into(),
            reveal_step: 0,
            source: None,
            kind: BlockKind::Image {
                destination: "diagram".into(),
                alt: "prepared Mermaid".into(),
                rows: 3,
            },
        };
        let element = rich.build(&block).unwrap();
        let layout = element.layout(Constraints::for_width(10), &mut LayoutCx::default());
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 10, 5));
        terminal.draw(|cx| element.paint(&layout, cx)).unwrap();
        let scene = terminal.image_scene().placements().next().unwrap();
        assert!(
            matches!(&scene.payload, ImagePayload::Png { width: 8, height: 8, bytes } if !bytes.is_empty())
        );
    }
    #[test]
    fn image_provider_contributes_real_image_scene() {
        let mut rich = RichAssets::default();
        std::sync::Arc::make_mut(&mut rich.images).insert(
            "asset".into(),
            ImagePayload::Pixels {
                bytes: vec![255; 4],
                width: 1,
                height: 1,
                format: bmux_tui::image::ImagePixelFormat::Rgba8,
            },
        );
        let block = Block {
            id: "image".into(),
            reveal_step: 0,
            source: None,
            kind: BlockKind::Image {
                destination: "asset".into(),
                alt: "example".into(),
                rows: 3,
            },
        };
        let element = rich.build(&block).unwrap();
        let layout = element.layout(Constraints::for_width(10), &mut LayoutCx::default());
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 10, 5));
        terminal.draw(|cx| element.paint(&layout, cx)).unwrap();
        assert_eq!(terminal.image_scene().placements().count(), 1);
    }
}
