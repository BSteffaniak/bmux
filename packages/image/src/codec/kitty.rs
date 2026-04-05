//! Kitty graphics protocol codec.
//!
//! Kitty graphics uses APC sequences: `ESC _ G <key>=<value>;... ; <payload> ESC \`
//! The first byte after `ESC _ ` is always `G`.

use crate::model::{
    ImagePosition, KittyCommand, KittyDeleteSpecifier, KittyFormat, KittyPlacement, KittySourceRect,
};

/// Parse a kitty graphics APC body (bytes after `G`, before ST).
///
/// The body format is: `key=value,key=value,...;base64-payload`
///
/// Returns `None` if the body is not a valid kitty graphics command.
pub fn parse_command(body: &[u8], cursor_pos: ImagePosition) -> Option<KittyCommand> {
    // The body starts with 'G' — skip it.
    let body = if body.first() == Some(&b'G') {
        &body[1..]
    } else {
        return None;
    };

    // Split on ';' to separate headers from payload.
    let (headers, payload) = match body.iter().position(|&b| b == b';') {
        Some(pos) => (&body[..pos], &body[pos + 1..]),
        None => (body, &[] as &[u8]),
    };

    // Parse key=value pairs from headers.
    let headers_str = std::str::from_utf8(headers).ok()?;
    let mut params = std::collections::HashMap::new();
    for pair in headers_str.split(',') {
        if let Some((k, v)) = pair.split_once('=') {
            params.insert(k, v);
        }
    }

    let action = params.get("a").and_then(|s| s.as_bytes().first().copied());

    match action {
        // Transmit (default if no action specified, or a=t, a=T)
        None | Some(b't') | Some(b'T') => {
            let image_id = parse_u32(&params, "i").unwrap_or(0);
            let format = match parse_u32(&params, "f").unwrap_or(32) {
                24 => KittyFormat::Rgb,
                32 => KittyFormat::Rgba,
                100 => KittyFormat::Png,
                _ => KittyFormat::Rgba,
            };
            let width = parse_u32(&params, "s").unwrap_or(0);
            let height = parse_u32(&params, "v").unwrap_or(0);
            let more_chunks = params.get("m").map(|v| *v == "1").unwrap_or(false);

            // Decode base64 payload.
            let data = base64_decode(payload);

            Some(KittyCommand::Transmit {
                image_id,
                format,
                data,
                width,
                height,
                more_chunks,
            })
        }

        // Place
        Some(b'p') => {
            let image_id = parse_u32(&params, "i").unwrap_or(0);
            let placement_id = parse_u32(&params, "p").unwrap_or(0);
            let z_index = params
                .get("z")
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or(0);

            // Position: use cursor position if not specified in the command.
            let col = parse_u32(&params, "C").map(|v| v as u16);
            let row = parse_u32(&params, "R").map(|v| v as u16);
            let position = ImagePosition {
                row: row.unwrap_or(cursor_pos.row),
                col: col.unwrap_or(cursor_pos.col),
            };

            // Source rectangle for sub-image display.
            let source_rect = if params.contains_key("x")
                || params.contains_key("y")
                || params.contains_key("w")
                || params.contains_key("h")
            {
                Some(KittySourceRect {
                    x: parse_u32(&params, "x").unwrap_or(0),
                    y: parse_u32(&params, "y").unwrap_or(0),
                    width: parse_u32(&params, "w").unwrap_or(0),
                    height: parse_u32(&params, "h").unwrap_or(0),
                })
            } else {
                None
            };

            Some(KittyCommand::Place(KittyPlacement {
                image_id,
                placement_id,
                position,
                source_rect,
                z_index,
            }))
        }

        // Delete
        Some(b'd') => {
            let specifier =
                if let Some(what) = params.get("d").and_then(|v| v.as_bytes().first().copied()) {
                    match what {
                        b'a' | b'A' => KittyDeleteSpecifier::All,
                        b'i' | b'I' => {
                            let id = parse_u32(&params, "i").unwrap_or(0);
                            KittyDeleteSpecifier::ByImageId(id)
                        }
                        _ => KittyDeleteSpecifier::All,
                    }
                } else {
                    // Default: delete by image ID if 'i' is present.
                    if let Some(id) = parse_u32(&params, "i") {
                        KittyDeleteSpecifier::ByImageId(id)
                    } else {
                        KittyDeleteSpecifier::All
                    }
                };

            Some(KittyCommand::Delete { specifier })
        }

        // Query
        Some(b'q') => {
            let image_id = parse_u32(&params, "i").unwrap_or(0);
            Some(KittyCommand::Query { image_id })
        }

        _ => None,
    }
}

/// Encode a kitty graphics transmit command as APC body bytes
/// (between `ESC _` and `ESC \`).
pub fn encode_transmit(
    image_id: u32,
    format: KittyFormat,
    data: &[u8],
    width: u32,
    height: u32,
) -> Vec<u8> {
    let fmt = match format {
        KittyFormat::Rgb => 24,
        KittyFormat::Rgba => 32,
        KittyFormat::Png => 100,
    };
    let b64 = base64_encode(data);
    format!("Ga=t,i={image_id},f={fmt},s={width},v={height};{b64}").into_bytes()
}

/// Encode a kitty graphics placement command.
pub fn encode_place(image_id: u32, placement_id: u32, row: u16, col: u16) -> Vec<u8> {
    format!("Ga=p,i={image_id},p={placement_id},C={col},R={row}").into_bytes()
}

fn parse_u32(params: &std::collections::HashMap<&str, &str>, key: &str) -> Option<u32> {
    params.get(key).and_then(|v| v.parse().ok())
}

/// Simple base64 decoder (standard alphabet, no padding required).
pub fn base64_decode(input: &[u8]) -> Vec<u8> {
    const TABLE: [u8; 256] = {
        let mut t = [255u8; 256];
        let mut i = 0u8;
        loop {
            if i >= 26 {
                break;
            }
            t[(b'A' + i) as usize] = i;
            t[(b'a' + i) as usize] = i + 26;
            i += 1;
        }
        i = 0;
        loop {
            if i >= 10 {
                break;
            }
            t[(b'0' + i) as usize] = i + 52;
            i += 1;
        }
        t[b'+' as usize] = 62;
        t[b'/' as usize] = 63;
        t
    };

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut accum: u32 = 0;
    let mut bits: u32 = 0;

    for &byte in input {
        let val = TABLE[byte as usize];
        if val == 255 {
            continue; // skip padding and whitespace
        }
        accum = (accum << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accum >> bits) as u8);
            accum &= (1 << bits) - 1;
        }
    }

    out
}

/// Simple base64 encoder (standard alphabet with padding).
pub fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_transmit_command() {
        let body = b"Ga=t,i=42,f=32,s=100,v=50;AAAA";
        let pos = ImagePosition { row: 0, col: 0 };
        let cmd = parse_command(body, pos).unwrap();
        match cmd {
            KittyCommand::Transmit {
                image_id,
                format,
                width,
                height,
                ..
            } => {
                assert_eq!(image_id, 42);
                assert_eq!(format, KittyFormat::Rgba);
                assert_eq!(width, 100);
                assert_eq!(height, 50);
            }
            _ => panic!("expected Transmit"),
        }
    }

    #[test]
    fn parse_place_command() {
        let body = b"Ga=p,i=42,p=1,C=10,R=5";
        let pos = ImagePosition { row: 0, col: 0 };
        let cmd = parse_command(body, pos).unwrap();
        match cmd {
            KittyCommand::Place(placement) => {
                assert_eq!(placement.image_id, 42);
                assert_eq!(placement.placement_id, 1);
                assert_eq!(placement.position.col, 10);
                assert_eq!(placement.position.row, 5);
            }
            _ => panic!("expected Place"),
        }
    }

    #[test]
    fn parse_delete_all() {
        let body = b"Ga=d,d=a";
        let pos = ImagePosition { row: 0, col: 0 };
        let cmd = parse_command(body, pos).unwrap();
        match cmd {
            KittyCommand::Delete {
                specifier: KittyDeleteSpecifier::All,
            } => {}
            _ => panic!("expected Delete All"),
        }
    }

    #[test]
    fn base64_roundtrip() {
        let original = b"Hello, world! This is a test of base64.";
        let encoded = base64_encode(original);
        let decoded = base64_decode(encoded.as_bytes());
        assert_eq!(decoded, original);
    }
}
