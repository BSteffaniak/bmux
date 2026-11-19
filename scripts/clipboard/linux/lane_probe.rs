//! Copy into packages/clipboard/examples/ in a disposable source tree only.
use bmux_clipboard::{copy_png_image, copy_text, read_payload, read_text, ClipboardPayload};
use std::io::{Read, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BMUX_CLIPBOARD_ISOLATED_TEST").as_deref() != Ok("1") {
        return Err("refusing to probe a non-test clipboard".into());
    }
    let args: Vec<_> = std::env::args().collect();
    let path = &args[2];
    match args[1].as_str() {
        "write-text" => copy_text(&std::fs::read_to_string(path)?)?,
        "write-image" => copy_png_image(&std::fs::read(path)?)?,
        "read-text" => return Ok(std::fs::write(path, read_text()?)?),
        "read-image" => match read_payload(true)? {
            ClipboardPayload::ImagePng(bytes) => return Ok(std::fs::write(path, bytes)?),
            _ => return Err("no image".into()),
        },
        _ => return Err("unknown operation".into()),
    }
    println!("ready");
    std::io::stdout().flush()?;
    // Closing stdin models an ordinary writer process exit.
    std::io::stdin().read_exact(&mut [0]).or_else(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Ok(())
        } else {
            Err(e)
        }
    })?;
    Ok(())
}
