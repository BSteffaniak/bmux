//! Bounded preparation, performed before presentation or on a preparation worker.
use anyhow::{Context, Result, ensure};
use bmux_presentation::custom::RichContent;
use bmux_presentation::{Block, BlockKind};
use bmux_presentation_runtime::Asset;
use bmux_tui::component::Element;
use bmux_tui::image::ImagePayload;
use std::collections::BTreeMap;
include!("highlight.rs");

#[derive(Default)]
pub(crate) struct RichAssets {
    pub images: BTreeMap<String, ImagePayload>,
    pub code: BTreeMap<String, bmux_tui::text::Text>,
}
impl RichAssets {
    pub fn prepare(assets: &[Asset]) -> Result<Self> {
        let mut images = BTreeMap::new();
        let mut decoded_bytes = 0_usize;
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
                ImagePayload::Pixels {
                    bytes: decoded.to_rgba8().into_raw(),
                    width: decoded.width(),
                    height: decoded.height(),
                    format: bmux_tui::image::ImagePixelFormat::Rgba8,
                }
            };
            let size = match &payload {
                ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes.len(),
            };
            decoded_bytes = decoded_bytes
                .checked_add(size)
                .context("decoded asset size overflow")?;
            ensure!(
                decoded_bytes <= 64 * 1024 * 1024,
                "decoded asset bundle exceeds memory budget"
            );
            images.insert(asset.id.clone(), payload);
        }
        Ok(Self {
            images,
            code: BTreeMap::new(),
        })
    }
}
impl RichContent for RichAssets {
    fn build<'a>(&'a self, block: &'a Block) -> Option<Element<'a>> {
        if let BlockKind::Code { .. } = &block.kind {
            return self.code.get(block.id.as_str()).map(|text| {
                Element::new(
                    bmux_tui::prelude::TextBlock::new(text.clone())
                        .id(format!("code/{}", block.id.as_str())),
                )
            });
        }
        let BlockKind::Image {
            destination, rows, ..
        } = &block.kind
        else {
            return None;
        };
        self.images.get(destination).map(|payload| {
            Element::new(bmux_presentation::component::ImageComponent::new(
                format!("presentation/image/{}", block.id.as_str()),
                payload.clone(),
                *rows,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::component::{Constraints, LayoutCx};
    use bmux_tui::{geometry::Rect, terminal::Terminal};
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
        rich.images.insert(
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
