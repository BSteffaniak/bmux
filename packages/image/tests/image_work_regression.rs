//! Deterministic public-API checks: never assert elapsed time.
#[path = "../benches/support/mod.rs"]
mod support;

#[cfg(feature = "sixel")]
#[path = "../benches/support/sixel_reference.rs"]
mod sixel_reference;

use bmux_image::codec::base64::{base64_decode, base64_encode};

#[test]
fn fixture_selector_is_fixed_and_validated() {
    for kind in ["palette", "solid", "gradient", "transparent", "detailed"] {
        let fixture = support::fixture(17, 13, kind).expect("known fixture");
        assert_eq!(
            fixture,
            support::fixture(17, 13, kind).expect("fixed fixture")
        );
        assert_eq!(fixture.data.len(), 17 * 13 * 4);
    }
    let transparent = support::fixture(17, 13, "transparent").expect("known fixture");
    for alpha in [0, 127, 128, 255] {
        assert!(
            transparent
                .data
                .chunks_exact(4)
                .any(|pixel| pixel[3] == alpha)
        );
    }
    assert!(support::fixture(17, 13, "unknown").is_err());
}

#[cfg(feature = "sixel")]
#[test]
fn optimized_sixel_matches_frozen_reference_for_selected_fixtures() {
    for kind in ["palette", "solid", "gradient", "transparent", "detailed"] {
        for (width, height) in [(1, 1), (17, 5), (17, 6), (17, 7), (33, 13)] {
            let fixture = support::fixture(width, height, kind).expect("known fixture");
            assert_eq!(
                bmux_image::codec::sixel::encode(&fixture),
                sixel_reference::encode(&fixture),
                "{kind} {width}x{height}"
            );
        }
    }
}

#[test]
fn base64_exact_lengths_and_roundtrip_at_chunk_boundaries() {
    for len in [0, 1, 2, 3, 3071, 3072, 3073, 6144] {
        let source: Vec<_> = (0_u8..=255).cycle().take(len).collect();
        let encoded = base64_encode(&source);
        assert_eq!(encoded.len(), len.div_ceil(3) * 4);
        assert_eq!(base64_decode(encoded.as_bytes()), source);
    }
}

#[test]
fn fixture_has_fixed_dimensions_and_opaque_pixels() {
    let pixels = support::pixels(17, 13);
    assert_eq!(pixels.data.len(), 17 * 13 * 4);
    assert!(pixels.data.chunks_exact(4).all(|pixel| pixel[3] == 255));
    assert_eq!(support::count_marker(b"abcabc", b"abc"), 2);
}

#[cfg(feature = "kitty")]
#[test]
fn kitty_chunk_budget_final_marker_and_exact_payload() {
    use bmux_image::codec::kitty::encode_transmit_chunks;
    use bmux_image::model::KittyFormat;
    for len in [0, 1, 3071, 3072, 3073, 6144] {
        let source: Vec<_> = (0_u8..=255).cycle().take(len).collect();
        let chunks = encode_transmit_chunks(7, KittyFormat::Rgba, &source, 17, 13);
        assert_eq!(chunks.len(), len.div_ceil(3072).max(1));
        let mut decoded = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let separator = chunk
                .iter()
                .position(|byte| *byte == b';')
                .expect("header delimiter");
            let header = std::str::from_utf8(&chunk[..separator]).expect("ASCII header");
            let more = usize::from(index + 1 < chunks.len());
            assert_eq!(header, format!("Ga=t,i=7,f=32,s=17,v=13,m={more},q=2"));
            let payload = &chunk[separator + 1..];
            assert!(payload.len() <= 4096);
            decoded.extend(base64_decode(payload));
        }
        assert_eq!(decoded, source);
    }
}

#[cfg(feature = "iterm2")]
#[test]
fn iterm2_encoder_roundtrip_and_byte_count() {
    use bmux_image::codec::iterm2::{encode_body, parse_body};
    let source = support::pixels(17, 13).data;
    let encoded = encode_body(&source, true);
    assert_eq!(
        encoded.len(),
        b"inline=1:".len() + source.len().div_ceil(3) * 4
    );
    let (params, decoded) = parse_body(&encoded).expect("encoded body parses");
    assert!(params.inline);
    assert_eq!(decoded, source);
}

#[cfg(feature = "sixel")]
#[test]
fn sixel_palette_fixture_roundtrip_partial_bands() {
    use bmux_image::codec::sixel::{decode, encode};
    for (width, height) in [(1, 1), (17, 5), (17, 6), (17, 7), (17, 13)] {
        let pixels = support::pixels(width, height);
        let encoded = encode(&pixels).expect("valid opaque fixture");
        assert_eq!(encoded, encode(&pixels).expect("deterministic encoding"));
        assert_eq!(decode(&encoded).expect("encoded fixture decodes"), pixels);
    }
}

#[cfg(feature = "kitty")]
#[test]
fn retained_attach_identity_avoids_upload_until_new_id() {
    use bmux_image::HostImageCapabilities;
    use bmux_image::compositor::{KittyHostState, PaneRect, render_pane_images};
    use bmux_image::config::ImageDecodeMode;
    use bmux_image::model::{
        ImageCellSize, ImagePayload, ImagePixelSize, ImagePosition, ImageProtocol, PaneImage,
    };
    let pixels = support::pixels(17, 13);
    let mut image = PaneImage {
        id: 7,
        protocol: ImageProtocol::KittyGraphics,
        payload: ImagePayload {
            raw: None,
            pixels: Some(pixels),
        },
        position: ImagePosition { row: 1, col: 2 },
        cell_size: ImageCellSize { rows: 3, cols: 5 },
        pixel_size: ImagePixelSize {
            width: 17,
            height: 13,
        },
    };
    let caps = HostImageCapabilities {
        kitty_graphics: true,
        ..Default::default()
    };
    let pane = PaneRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let mut state = KittyHostState::default();
    for (id, col, expected_uploads) in [(7, 2, 1), (7, 2, 0), (7, 3, 0), (8, 3, 1)] {
        image.id = id;
        image.position.col = col;
        let mut out = Vec::new();
        render_pane_images(
            &mut out,
            std::slice::from_ref(&image),
            pane,
            &caps,
            ImageDecodeMode::Server,
            &mut state,
        )
        .expect("Vec output cannot fail");
        assert_eq!(support::count_marker(&out, b"\x1b_Ga=t,"), expected_uploads);
        assert_eq!(support::count_marker(&out, b"\x1b_Ga=p,"), 1);
    }
    let mut out = Vec::new();
    state.reset(&mut out).expect("Vec cleanup cannot fail");
    assert_eq!(support::count_marker(&out, b"\x1b_Ga=d,"), 2);
    assert!(state.transmitted.is_empty());
}

#[cfg(feature = "kitty")]
#[test]
fn clipped_frames_reuse_decode_and_preparation_then_repair_uploads() {
    use bmux_image::HostImageCapabilities;
    use bmux_image::compositor::{KittyHostState, PaneRect, render_pane_images_clipped};
    use bmux_image::model::{
        ImageCellSize, ImagePayload, ImagePixelSize, ImagePosition, ImageProtocol, PaneImage,
    };
    let image = PaneImage {
        id: 11,
        protocol: ImageProtocol::KittyGraphics,
        payload: ImagePayload {
            raw: None,
            pixels: Some(support::pixels(17, 13)),
        },
        position: ImagePosition { row: 1, col: 2 },
        cell_size: ImageCellSize { rows: 3, cols: 5 },
        pixel_size: ImagePixelSize {
            width: 17,
            height: 13,
        },
    };
    let caps = HostImageCapabilities {
        kitty_graphics: true,
        ..Default::default()
    };
    let pane = PaneRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let mut committed = KittyHostState::default();
    for expected_uploads in [1, 0] {
        let mut candidate = committed.clone();
        candidate.begin_frame();
        let mut out = Vec::new();
        render_pane_images_clipped(
            &mut out,
            std::slice::from_ref(&image),
            pane,
            &[],
            &caps,
            &mut candidate,
        )
        .expect("valid fixture");
        candidate
            .finish_frame(&mut out)
            .expect("Vec output cannot fail");
        assert_eq!(support::count_marker(&out, b"\x1b_Ga=t,"), expected_uploads);
        assert_eq!(candidate.work_counts(), (1, 1));
        committed = candidate;
    }
    struct FailAfter {
        remaining: usize,
    }
    impl std::io::Write for FailAfter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::other("injected partial output failure"));
            }
            let accepted = bytes.len().min(self.remaining);
            self.remaining -= accepted;
            Ok(accepted)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut uncertain = committed.clone();
    uncertain.begin_frame();
    let mut failing = FailAfter { remaining: 10 };
    assert!(
        render_pane_images_clipped(
            &mut failing,
            std::slice::from_ref(&image),
            pane,
            &[],
            &caps,
            &mut uncertain,
        )
        .is_err()
    );
    assert_eq!(committed.work_counts(), (1, 1));
    committed.invalidate_after_failed_output(&uncertain);
    let mut candidate = committed.clone();
    candidate.begin_frame();
    let mut out = Vec::new();
    render_pane_images_clipped(
        &mut out,
        std::slice::from_ref(&image),
        pane,
        &[],
        &caps,
        &mut candidate,
    )
    .expect("repair fixture");
    candidate
        .finish_frame(&mut out)
        .expect("Vec output cannot fail");
    assert_eq!(support::count_marker(&out, b"\x1b_Ga=t,"), 1);
    assert!(support::count_marker(&out, b"\x1b_Ga=d,") > 0);
    assert_eq!(candidate.work_counts(), (1, 1));
    // No candidate publication occurred until all frame bytes were accepted.
    assert_eq!(committed.work_counts(), (1, 1));
}

#[cfg(feature = "kitty")]
#[test]
fn tui_scene_warm_movement_hide_reveal_replacement_and_cleanup() {
    use bmux_image::compositor::PaneRect;
    use bmux_image::tui::TuiImageCompositor;
    use bmux_image::{HostImageCapabilities, ImageConfig};
    use bmux_tui::geometry::Rect;
    use bmux_tui::image::{
        ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
    };
    use bmux_tui::image_scene::ImageScene;
    let mut placement = ImagePlacement {
        key: ImageKey::new("retained"),
        payload: ImagePayload::Pixels {
            bytes: support::pixels(17, 13).data,
            width: 17,
            height: 13,
            format: ImagePixelFormat::Rgba8,
        },
        destination: Rect::new(2, 1, 5, 3),
        clip: Rect::new(0, 0, 80, 24),
        lifecycle: ImageLifecycle::Persistent,
    };
    let mut scene = ImageScene::default();
    let mut compositor = TuiImageCompositor::new();
    let caps = HostImageCapabilities {
        kitty_graphics: true,
        ..Default::default()
    };
    let config = ImageConfig::default();
    let terminal = PaneRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    for (phase, expected_uploads) in [
        ("cold", 1),
        ("warm", 0),
        ("movement", 0),
        ("hide", 0),
        ("reveal", 0),
        ("replacement", 1),
    ] {
        match phase {
            "movement" => placement.destination.x += 1,
            "hide" => placement.clip = Rect::new(0, 0, 0, 0),
            "reveal" => placement.clip = Rect::new(0, 0, 80, 24),
            "replacement" => {
                if let ImagePayload::Pixels { bytes, .. } = &mut placement.payload {
                    bytes[0] ^= 255;
                }
            }
            _ => {}
        }
        let delta = scene.reconcile(&[ImageContribution::Present(placement.clone())]);
        if phase == "warm" {
            assert!(delta.upserted.is_empty());
        }
        compositor.apply_delta(&delta);
        let mut candidate = compositor.clone();
        let mut out = Vec::new();
        candidate
            .render(&mut out, &scene, terminal, &caps, &config)
            .expect("valid scene");
        assert_eq!(
            support::count_marker(&out, b"\x1b_Ga=t,"),
            expected_uploads,
            "{phase}"
        );
        if phase == "hide" {
            assert_eq!(support::count_marker(&out, b"\x1b_Ga=p,"), 0);
        }
        compositor = candidate;
    }
    let delta = scene.reconcile(&[ImageContribution::Remove(placement.key)]);
    assert_eq!(delta.removed.len(), 1);
    compositor.apply_delta(&delta);
    let mut out = Vec::new();
    compositor
        .render(&mut out, &scene, terminal, &caps, &config)
        .expect("empty scene");
    assert_eq!(support::count_marker(&out, b"\x1b_Ga=t,"), 0);
    assert_eq!(support::count_marker(&out, b"\x1b_Ga=p,"), 0);
    compositor.cleanup(&mut out, &caps).expect("cleanup");
}
