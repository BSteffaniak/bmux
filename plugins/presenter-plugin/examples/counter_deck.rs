//! Build a linked Rust deck without running arbitrary viewer-supplied code.
//! cargo run -p bmux_presenter_plugin --example counter_deck > counter.json
use bmux_presenter::{DeckBuilder, SlideBuilder};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let deck = DeckBuilder::new("rust-counter")
        .slide(
            SlideBuilder::new("intro")
                .text("title", "# Rust presentation\nA linked DeckBuilder source")
                .pause()
                .text("reveal", "Next reveals this line")
                .build(),
        )
        .slide(
            SlideBuilder::new("counter")
                .text("heading", "# Interactive canonical counter")
                .custom("counter", "counter", serde_json::json!({}))
                .build(),
        )
        .build()
        .map_err(std::io::Error::other)?;
    println!("{}", serde_json::to_string_pretty(&deck)?);
    Ok(())
}
