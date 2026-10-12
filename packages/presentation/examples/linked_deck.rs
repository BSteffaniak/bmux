//! Linked application with caller-owned interactive BMUX button state.
use bmux_presentation::{
    Block, CustomComponentFactory, CustomRegistry, DeckBuilder, ImageComponent, PresentationState,
    PresentationTheme, RichContent, SlideBuilder, ViewerComponent, ViewerState,
};
use bmux_tui::image::{ImagePayload, ImagePixelFormat};
use bmux_tui::prelude::{
    Buffer, Component, Constraints, Element, Frame, LayoutCx, PaintCx, Rect, Size,
};
use bmux_tui_components::button::{ButtonComponent, ButtonState};
use std::cell::Cell;

struct DemoButton {
    state: Cell<ButtonState>,
}
impl CustomComponentFactory for DemoButton {
    fn build<'a>(&'a self, block: &'a Block) -> Result<Element<'a>, String> {
        Ok(Element::new(ButtonComponent::new(
            format!("demo/{}", block.id.as_str()),
            "Interactive demo",
            &self.state,
        )))
    }
    fn revision(&self) -> bmux_tui::component::ComponentRevision {
        ButtonComponent::new("demo", "Interactive demo", &self.state).revision()
    }
}
struct AuthorizedAssets;
impl RichContent for AuthorizedAssets {
    fn build<'a>(&'a self, block: &'a Block) -> Option<Element<'a>> {
        let bmux_presentation::BlockKind::Image {
            destination, rows, ..
        } = &block.kind
        else {
            return None;
        };
        if destination != "asset:demo" {
            return None;
        }
        Some(Element::new(ImageComponent::new(
            format!("demo/image/{}", block.id.as_str()),
            ImagePayload::Pixels {
                bytes: vec![0, 128, 255, 255],
                width: 1,
                height: 1,
                format: ImagePixelFormat::Rgba8,
            },
            *rows,
        )))
    }
}
fn main() -> Result<(), String> {
    let deck = DeckBuilder::new("linked-demo")
        .slide(
            SlideBuilder::new("intro")
                .markdown("title", "# Linked BMUX presentation")
                .custom("button", "demo-button", serde_json::json!({}))
                .pause()
                .block(
                    "image",
                    bmux_presentation::BlockKind::Image {
                        destination: "asset:demo".into(),
                        alt: "Authorized image".into(),
                        rows: 4,
                    },
                )
                .notes("Local speaker notes never enter the audience viewer.")
                .build(),
        )
        .build()?;
    let navigation = PresentationState::new(&deck);
    let state = ViewerState::new();
    let theme = PresentationTheme::dark();
    let mut registry = CustomRegistry::new();
    registry.register(
        "demo-button",
        DemoButton {
            state: Cell::new(ButtonState::new()),
        },
    )?;
    let assets = AuthorizedAssets;
    let viewer =
        ViewerComponent::new(&deck, &navigation, &state, &theme, &registry).rich_content(&assets);
    let area = Rect::new(0, 0, 80, 24);
    let layout = viewer.layout(
        Constraints::tight(Size::new(80, 24)),
        &mut LayoutCx::default(),
    );
    let mut buffer = Buffer::empty(area);
    let mut frame = Frame::new(&mut buffer);
    viewer.paint(&layout, &mut PaintCx::new(&mut frame));
    // A runtime presents this canonical frame and routes input using this same layout.
    println!(
        "{} slides; authoritative viewport {}x{}",
        deck.slides.len(),
        layout.size.width,
        layout.size.height
    );
    Ok(())
}
