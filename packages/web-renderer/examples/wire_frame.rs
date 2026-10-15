//! Convert diagnostic JSON to binary, or inspect an inline binary snapshot.
use std::io::{Read, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = Vec::new();
    std::io::stdin()
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut input)?;
    let mode = std::env::args().nth(1);
    if mode.as_deref() == Some("--delta") {
        let path = std::env::args()
            .nth(2)
            .ok_or("--delta requires a base snapshot path")?;
        let base = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        base.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        let base = bmux_web_renderer::decode_binary(&bytes, None)?;
        let target = bmux_web_renderer::decode_binary(&input, None)?;
        std::io::stdout().write_all(&bmux_web_renderer::encode_delta(&target, &base)?)?;
    } else if mode.as_deref() == Some("--dense") {
        let frame = bmux_web_renderer::decode_binary(&input, None)?;
        let mut value = serde_json::to_value(frame)?;
        value["generation"] = "dense-large-conformance".into();
        value["revision"] = 1.into();
        for cell in value["buffer"]["cells"]
            .as_array_mut()
            .ok_or("missing cells")?
        {
            cell["symbol"] = "X".into();
            cell["width"] = 1.into();
            cell["wide_continuation"] = false.into();
        }
        let frame: bmux_web_renderer::BrowserFrame = serde_json::from_value(value)?;
        std::io::stdout().write_all(&bmux_web_renderer::encode_binary(&frame)?)?;
    } else if std::env::args().nth(1).as_deref() == Some("--inspect") {
        let frame = bmux_web_renderer::decode_binary(&input, None)?;
        println!(
            "{}",
            serde_json::json!({
                "area": frame.buffer().area(),
                "revision": frame.revision().to_string(),
                "x_cells": frame.buffer().cells().iter().filter(|cell| cell.symbol == "X").count(),
            })
        );
    } else {
        let input = std::str::from_utf8(&input)?;
        let frame = bmux_web_renderer::decode_json(input, bmux_web_renderer::Limits::default())?;
        std::io::stdout().write_all(&bmux_web_renderer::encode_binary(&frame)?)?;
    }
    Ok(())
}
