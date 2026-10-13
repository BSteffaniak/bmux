//! Deterministic transport wire equivalence and roundtrips, without timing gates.

use bmux_image::codec::base64::{base64_decode, base64_encode, base64_encode_into};

// Independent bit-stream reference, rather than the production triple encoder.
fn reference_base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    let mut accumulator = 0u32;
    let mut bits = 0;
    for &byte in data {
        accumulator = (accumulator << 8) | u32::from(byte);
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            encoded.push(char::from(ALPHABET[((accumulator >> bits) & 63) as usize]));
        }
        accumulator &= (1 << bits) - 1;
    }
    if bits > 0 {
        encoded.push(char::from(ALPHABET[(accumulator << (6 - bits)) as usize]));
    }
    while !encoded.len().is_multiple_of(4) {
        encoded.push('=');
    }
    encoded
}

fn generated_data(len: usize) -> Vec<u8> {
    let mut state = 0x65a9_e372u32;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .collect()
}

#[test]
fn base64_known_vectors_append_preserves_prefix_and_empty_input() {
    for (raw, expected) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        let mut out = b"\xffheader;".to_vec();
        base64_encode_into(raw.as_bytes(), &mut out);
        assert_eq!(&out[..8], b"\xffheader;");
        assert_eq!(&out[8..], expected.as_bytes());
        assert_eq!(base64_encode(raw.as_bytes()), expected);
        assert_eq!(base64_decode(&out[8..]), raw.as_bytes());
        let before = out.clone();
        base64_encode_into(b"", &mut out);
        assert_eq!(out, before);
    }
}

#[test]
fn base64_generated_and_all_bytes_match_reference_and_roundtrip() {
    let all_bytes: Vec<u8> = (0..=255).collect();
    let generated = generated_data(12_289);
    for data in [&all_bytes[..], &generated[..]] {
        for len in 0..=data.len() {
            // Cover every short length plus all residues near transport boundaries.
            if len > 260
                && ![
                    3070, 3071, 3072, 3073, 3074, 6143, 6144, 6145, 12_288, 12_289,
                ]
                .contains(&len)
            {
                continue;
            }
            let data = &data[..len];
            let expected = reference_base64(data);
            let mut out = b"prefix:".to_vec();
            base64_encode_into(data, &mut out);
            assert_eq!(&out[7..], expected.as_bytes(), "len={len}");
            assert_eq!(base64_encode(data), expected, "len={len}");
            assert_eq!(base64_decode(&out[7..]), data, "len={len}");
        }
    }
}

#[test]
fn base64_append_reuses_sufficient_final_buffer_capacity() {
    let data = generated_data(3072);
    let mut out = Vec::with_capacity(7 + 4096);
    out.extend_from_slice(b"header;");
    let allocation = out.as_ptr();
    let capacity = out.capacity();
    base64_encode_into(&data, &mut out);
    assert_eq!(out.as_ptr(), allocation);
    assert_eq!(out.capacity(), capacity);
    assert_eq!(out.len(), 7 + 4096);
    assert_eq!(base64_decode(&out[7..]), data);
}

#[cfg(feature = "iterm2")]
#[test]
fn iterm2_exact_bodies_and_parser_roundtrip() {
    use bmux_image::codec::iterm2::{encode_body, encode_body_with_cells, parse_body};

    for len in [0, 1, 2, 3, 4, 255, 3072, 3073, 8193] {
        let data = generated_data(len);
        let payload = reference_base64(&data);
        for inline in [false, true] {
            let body = encode_body(&data, inline);
            assert_eq!(
                body,
                format!("inline={}:{payload}", u8::from(inline)).as_bytes()
            );
            let (params, decoded) = parse_body(&body).expect("generated body parses");
            assert_eq!(params.inline, inline);
            assert!(params.preserve_aspect_ratio);
            assert_eq!(decoded, data);
        }
        for (columns, rows) in [(0, 0), (1, 2), (80, 24), (u16::MAX, u16::MAX)] {
            let body = encode_body_with_cells(&data, columns, rows);
            assert_eq!(
                body,
                format!("inline=1;width={columns};height={rows};preserveAspectRatio=0:{payload}")
                    .as_bytes()
            );
            let (params, decoded) = parse_body(&body).expect("sized body parses");
            assert!(params.inline);
            assert!(!params.preserve_aspect_ratio);
            assert_eq!(params.width.as_deref(), Some(columns.to_string().as_str()));
            assert_eq!(params.height.as_deref(), Some(rows.to_string().as_str()));
            assert_eq!(decoded, data);
        }
    }
}

#[cfg(feature = "kitty")]
#[test]
fn kitty_exact_transmit_and_bounded_chunks_roundtrip() {
    use bmux_image::codec::kitty::{encode_transmit, encode_transmit_chunks, parse_command};
    use bmux_image::model::{ImagePosition, KittyCommand, KittyFormat};

    for (format, wire_format) in [
        (KittyFormat::Rgb, 24),
        (KittyFormat::Rgba, 32),
        (KittyFormat::Png, 100),
    ] {
        for len in [
            0, 1, 2, 3, 4, 3070, 3071, 3072, 3073, 3074, 6143, 6144, 6145, 12_289,
        ] {
            let data = generated_data(len);
            for (image_id, width, height) in
                [(0, 0, 0), (42, 320, 160), (u32::MAX, u32::MAX, u32::MAX)]
            {
                let body = encode_transmit(image_id, format, &data, width, height);
                assert_eq!(
                    body,
                    format!(
                        "Ga=t,i={image_id},f={wire_format},s={width},v={height},q=2;{}",
                        reference_base64(&data)
                    )
                    .as_bytes()
                );
                let Some(KittyCommand::Transmit {
                    data: decoded,
                    more_chunks,
                    ..
                }) = parse_command(&body, ImagePosition { row: 0, col: 0 })
                else {
                    panic!("transmit body must parse");
                };
                assert_eq!(decoded, data);
                assert!(!more_chunks);

                let chunks = encode_transmit_chunks(image_id, format, &data, width, height);
                let count = len.div_ceil(3072).max(1);
                assert_eq!(chunks.len(), count);
                let mut reconstructed = Vec::new();
                for (index, chunk) in chunks.iter().enumerate() {
                    let start = index * 3072;
                    let raw = &data[start..data.len().min(start + 3072)];
                    let more = u8::from(index + 1 < count);
                    let expected = format!(
                        "Ga=t,i={image_id},f={wire_format},s={width},v={height},m={more},q=2;{}",
                        reference_base64(raw)
                    );
                    assert_eq!(chunk, expected.as_bytes());
                    let separator = chunk.iter().position(|&byte| byte == b';').unwrap();
                    assert!(chunk.len() - separator - 1 <= 4096);
                    let Some(KittyCommand::Transmit {
                        image_id: parsed_id,
                        format: parsed_format,
                        data: decoded,
                        width: parsed_width,
                        height: parsed_height,
                        more_chunks,
                    }) = parse_command(chunk, ImagePosition { row: 0, col: 0 })
                    else {
                        panic!("chunk must parse");
                    };
                    assert_eq!(parsed_id, image_id);
                    assert_eq!(parsed_format, format);
                    assert_eq!((parsed_width, parsed_height), (width, height));
                    assert_eq!(more_chunks, more != 0);
                    assert_eq!(decoded, raw);
                    reconstructed.extend_from_slice(&decoded);
                }
                assert_eq!(reconstructed, data);
            }
        }
    }
}
