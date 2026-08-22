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
                        b'p' | b'P' => KittyDeleteSpecifier::ByPlacementId {
                            image_id: parse_u32(&params, "i").unwrap_or(0),
                            placement_id: parse_u32(&params, "p").unwrap_or(0),
                        },
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

const KITTY_MAX_ENCODED_CHUNK_BYTES: usize = 4096;
const KITTY_RAW_CHUNK_BYTES: usize = KITTY_MAX_ENCODED_CHUNK_BYTES / 4 * 3;

/// Encode a Kitty graphics transmission as bounded APC bodies.
///
/// Continuation chunks use `m=1`; the final chunk uses `m=0`. Each chunk
/// repeats identity and geometry so a nested terminal can validate and bound
/// every fragment independently.
#[must_use]
pub fn encode_transmit_chunks(
    image_id: u32,
    format: KittyFormat,
    data: &[u8],
    width: u32,
    height: u32,
) -> Vec<Vec<u8>> {
    let fmt = match format {
        KittyFormat::Rgb => 24,
        KittyFormat::Rgba => 32,
        KittyFormat::Png => 100,
    };
    let chunks = data.chunks(KITTY_RAW_CHUNK_BYTES).collect::<Vec<_>>();
    let chunk_count = chunks.len().max(1);
    (0..chunk_count)
        .map(|index| {
            let chunk = chunks.get(index).copied().unwrap_or_default();
            let more = u8::from(index + 1 < chunk_count);
            let b64 = base64_encode(chunk);
            format!("Ga=t,i={image_id},f={fmt},s={width},v={height},m={more},q=2;{b64}")
                .into_bytes()
        })
        .collect()
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
    // q=2 suppresses terminal replies. Without it, Kitty-compatible terminals
    // send APC acknowledgements on stdin, which attach clients may forward to
    // the running pane as literal text.
    format!("Ga=t,i={image_id},f={fmt},s={width},v={height},q=2;{b64}").into_bytes()
}

/// Encode a kitty graphics placement command for the current cursor cell.
pub fn encode_place(image_id: u32, placement_id: u32, _row: u16, _col: u16) -> Vec<u8> {
    encode_place_with_z(image_id, placement_id, 0)
}

/// Encode a kitty graphics placement command for the current cursor cell with a z-index.
pub fn encode_place_with_z(image_id: u32, placement_id: u32, z_index: i16) -> Vec<u8> {
    encode_place_with_z_and_cells(image_id, placement_id, z_index, 0, 0)
}

/// Encode a kitty graphics placement command for the current cursor cell with
/// z-index and optional cell extents.
pub fn encode_place_with_z_and_cells(
    image_id: u32,
    placement_id: u32,
    z_index: i16,
    columns: u16,
    rows: u16,
) -> Vec<u8> {
    // q=2 suppresses terminal replies for the same reason as transmit. BMUX
    // positions Kitty graphics by moving the cursor before placement; c/r only
    // describe the placement size in cells and avoid geometry-sized payloads.
    let mut command = format!("Ga=p,i={image_id},p={placement_id},z={z_index}");
    if columns > 0 {
        command.push_str(&format!(",c={columns}"));
    }
    if rows > 0 {
        command.push_str(&format!(",r={rows}"));
    }
    command.push_str(",q=2");
    command.into_bytes()
}

/// Encode a kitty graphics delete-by-image-id command.
pub fn encode_delete_image(image_id: u32) -> Vec<u8> {
    // q=2 suppresses delete acknowledgements too.
    format!("Ga=d,d=i,i={image_id},q=2").into_bytes()
}

/// Encode a kitty graphics delete-by-placement-id command.
pub fn encode_delete_placement(image_id: u32, placement_id: u32) -> Vec<u8> {
    // q=2 suppresses delete acknowledgements too.
    format!("Ga=d,d=p,i={image_id},p={placement_id},q=2").into_bytes()
}

fn parse_u32(params: &std::collections::HashMap<&str, &str>, key: &str) -> Option<u32> {
    params.get(key).and_then(|v| v.parse().ok())
}

pub use super::base64::{base64_decode, base64_encode};

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
    fn kitty_transmit_chunks_are_bounded_and_reassemble() {
        let data = vec![0x5a; 32 * 1024];
        let chunks = encode_transmit_chunks(42, KittyFormat::Png, &data, 320, 160);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.len() < 4300));
        for (index, chunk) in chunks.iter().enumerate() {
            let command = parse_command(chunk, ImagePosition { row: 0, col: 0 }).unwrap();
            let KittyCommand::Transmit {
                more_chunks,
                width,
                height,
                ..
            } = command
            else {
                panic!("expected transmit command");
            };
            assert_eq!(more_chunks, index + 1 < chunks.len());
            assert_eq!((width, height), (320, 160));
        }
    }

    #[test]
    fn encode_commands_suppress_terminal_replies() {
        let transmit = String::from_utf8(encode_transmit(42, KittyFormat::Rgba, b"abc", 1, 1))
            .expect("kitty transmit command should be utf8");
        assert!(transmit.contains(",q=2;"), "{transmit}");

        let place = String::from_utf8(encode_place(42, 42, 1, 2))
            .expect("kitty place command should be utf8");
        assert_eq!(place, "Ga=p,i=42,p=42,z=0,q=2");
        assert!(place.ends_with(",q=2"), "{place}");

        let sized_place = String::from_utf8(encode_place_with_z_and_cells(42, 43, 7, 12, 3))
            .expect("kitty sized place command should be utf8");
        assert_eq!(sized_place, "Ga=p,i=42,p=43,z=7,c=12,r=3,q=2");

        let delete = String::from_utf8(encode_delete_image(42))
            .expect("kitty delete command should be utf8");
        assert_eq!(delete, "Ga=d,d=i,i=42,q=2");

        let delete_placement = String::from_utf8(encode_delete_placement(42, 43))
            .expect("kitty delete placement command should be utf8");
        assert_eq!(delete_placement, "Ga=d,d=p,i=42,p=43,q=2");
    }

    #[test]
    fn parse_delete_placement() {
        let body = b"Ga=d,d=p,i=42,p=43";
        let pos = ImagePosition { row: 0, col: 0 };
        let cmd = parse_command(body, pos).unwrap();
        match cmd {
            KittyCommand::Delete {
                specifier:
                    KittyDeleteSpecifier::ByPlacementId {
                        image_id,
                        placement_id,
                    },
            } => {
                assert_eq!(image_id, 42);
                assert_eq!(placement_id, 43);
            }
            _ => panic!("expected Delete ByPlacementId"),
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
