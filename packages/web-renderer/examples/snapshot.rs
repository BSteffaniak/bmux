//! A standalone bmux_tui consumer that emits the exact browser snapshot format.
use bmux_tui::{
    damage::Damage,
    geometry::{Point, Rect},
    hit::HitRegion,
    image::{
        ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
    },
    paint::LocalRect,
    presentation::PresentationState,
    selection::{SelectionContentId, SelectionScope, plain_text_fragments},
    semantic::SemanticRegion,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use bmux_web_renderer::{BrowserFrame, encode_json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut state = PresentationState::new(Rect::new(0, 0, 80, 24));
    let title = "bmux_tui → browser · 界 · e\u{301} · 🦀";
    let text = std::collections::BTreeMap::from([(
        SelectionContentId::new("example:title"),
        title.to_owned(),
    )]);
    let json = state.present(
        Damage::Full,
        |cx| {
            cx.fill(
                LocalRect::new(0, 0, 80, 24),
                " ",
                Style::new().bg(Color::Rgb(16, 16, 16)),
            );
            cx.write_line(
                LocalRect::new(2, 1, 70, 1),
                &Line::from_spans(vec![Span::styled(
                    title,
                    Style::new()
                        .fg(Color::BrightCyan)
                        .add_modifier(Modifier::BOLD),
                )]),
            );
            cx.write_line(
                LocalRect::new(2, 3, 70, 1),
                &Line::from("Read-only shared presentation; no native transport required."),
            );
            cx.push_hit(HitRegion::new("example:title", Rect::new(2, 1, 40, 1)));
            cx.push_semantic(SemanticRegion::new(
                "example:title",
                Rect::new(2, 1, 40, 1),
                "heading",
            ));
            cx.push_selection_scope(SelectionScope::new("example:scope", Rect::new(2, 1, 40, 1)));
            for fragment in plain_text_fragments(
                "example:scope",
                "example:title",
                Rect::new(2, 1, 40, 1),
                0,
                title,
                0,
                1,
            ) {
                cx.push_selection_fragment(fragment);
            }
            cx.set_cursor(Point::new(2, 5), true);
            cx.push_image(ImageContribution::Present(ImagePlacement {
                fit: bmux_tui::image::ImageFit::default(),
                allocation: None,
                key: ImageKey::new("example:rgba"),
                payload: ImagePayload::Pixels {
                    bytes: vec![
                        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128,
                    ],
                    width: 2,
                    height: 2,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(2, 7, 8, 4),
                clip: Rect::new(0, 0, 80, 24),
                lifecycle: ImageLifecycle::Persistent,
            }));
        },
        |prepared, _previous| {
            encode_json(
                &BrowserFrame::from_prepared_in_generation("standalone-example", 1, prepared)?
                    .with_selection_text(&text)?,
            )
        },
    )?;
    if let Some(json) = json {
        match std::env::args().nth(1).as_deref() {
            None => println!("{json}"),
            Some("--binary") => {
                use std::io::Write;
                let frame =
                    bmux_web_renderer::decode_json(&json, bmux_web_renderer::Limits::default())?;
                std::io::stdout()
                    .lock()
                    .write_all(&bmux_web_renderer::encode_binary(&frame)?)?;
            }
            Some(_) => return Err("usage: snapshot [--binary]".into()),
        }
    }
    Ok(())
}
