//! Kitty graphics protocol codec.
//!
//! Kitty graphics uses APC sequences: `ESC _ G <key>=<value>;... ; <payload> ESC \`
//! The first byte after `ESC _ ` is always `G`.

use crate::model::{
    ImagePosition, KittyCommand, KittyDeleteSpecifier, KittyFormat, KittyPlacement, KittySourceRect,
};

/// Encode a placement with an integral source-pixel crop. Position remains
/// cursor-owned; cell dimensions describe the cropped output, not the full image.
#[must_use]
pub fn encode_place_with_crop(
    image_id: u32,
    placement_id: u32,
    cols: u16,
    rows: u16,
    crop: KittySourceRect,
) -> Vec<u8> {
    format!(
        "Ga=p,i={image_id},p={placement_id},z=0,c={cols},r={rows},q=2,x={},y={},w={},h={}",
        crop.x, crop.y, crop.width, crop.height
    )
    .into_bytes()
}

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

            // Kitty placements are anchored at the current cursor. C controls
            // cursor movement; it is not a column coordinate.
            let position = cursor_pos;
            let cell_size = crate::model::ImageCellSize {
                cols: u16::try_from(parse_u32(&params, "c").unwrap_or(0)).ok()?,
                rows: u16::try_from(parse_u32(&params, "r").unwrap_or(0)).ok()?,
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
                cell_size,
                source_rect,
                z_index,
            }))
        }

        // Delete
        Some(b'd') => {
            let specifier =
                if let Some(what) = params.get("d").and_then(|v| v.as_bytes().first().copied()) {
                    match what {
                        b'a' => KittyDeleteSpecifier::All,
                        b'A' => KittyDeleteSpecifier::AllAndData,
                        b'i' | b'I' => {
                            let id = parse_u32(&params, "i").unwrap_or(0);
                            if let Some(placement_id) = parse_u32(&params, "p") {
                                if what == b'I' {
                                    KittyDeleteSpecifier::ByPlacementIdAndData {
                                        image_id: id,
                                        placement_id,
                                    }
                                } else {
                                    KittyDeleteSpecifier::ByPlacementId {
                                        image_id: id,
                                        placement_id,
                                    }
                                }
                            } else if what == b'I' {
                                KittyDeleteSpecifier::ByImageIdAndData(id)
                            } else {
                                KittyDeleteSpecifier::ByImageId(id)
                            }
                        }
                        // Coordinate/number selectors are not placement-ID selectors.
                        // Reject unsupported selectors rather than deleting unrelated images.
                        _ => return None,
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
    let chunk_count = data.len().div_ceil(KITTY_RAW_CHUNK_BYTES).max(1);
    (0..chunk_count)
        .map(|index| {
            let start = index * KITTY_RAW_CHUNK_BYTES;
            let chunk = &data[start..data.len().min(start + KITTY_RAW_CHUNK_BYTES)];
            let more = u8::from(index + 1 < chunk_count);
            let mut body = format!("Ga=t,i={image_id},f={fmt},s={width},v={height},m={more},q=2;")
                .into_bytes();
            super::base64::base64_encode_into(chunk, &mut body);
            body
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
    // q=2 suppresses terminal replies. Without it, Kitty-compatible terminals
    // send APC acknowledgements on stdin, which attach clients may forward to
    // the running pane as literal text.
    let mut body = format!("Ga=t,i={image_id},f={fmt},s={width},v={height},q=2;").into_bytes();
    super::base64::base64_encode_into(data, &mut body);
    body
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
    format!("Ga=d,d=i,i={image_id},p={placement_id},q=2").into_bytes()
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
        let body = b"Ga=p,i=42,p=1,C=1,c=16,r=8";
        let pos = ImagePosition { row: 5, col: 10 };
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
        assert_eq!(delete_placement, "Ga=d,d=i,i=42,p=43,q=2");
    }

    #[test]
    fn crop_placement_round_trips_source_and_output_geometry() {
        let crop = KittySourceRect {
            x: 7,
            y: 11,
            width: 120,
            height: 80,
        };
        let encoded = encode_place_with_crop(42, 9, 12, 4, crop);
        let position = ImagePosition { row: 3, col: 5 };
        let Some(KittyCommand::Place(placement)) = parse_command(&encoded, position) else {
            panic!("expected placement");
        };
        assert_eq!((placement.image_id, placement.placement_id), (42, 9));
        assert_eq!(
            (placement.cell_size.cols, placement.cell_size.rows),
            (12, 4)
        );
        assert_eq!(placement.position, position);
        let decoded = placement.source_rect.unwrap();
        assert_eq!(
            (decoded.x, decoded.y, decoded.width, decoded.height),
            (7, 11, 120, 80)
        );
    }

    #[test]
    fn parse_delete_placement() {
        let body = b"Ga=d,d=i,i=42,p=43";
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
