//! Deterministic work and host-repair regression coverage; no timing gates.
use super::*;
use crate::model::{
    ImageCellSize, ImagePayload, ImagePixelSize, ImagePosition, ImageProtocol, PixelBuffer,
    PixelFormat,
};

fn image() -> PaneImage {
    PaneImage {
        id: 1,
        protocol: ImageProtocol::KittyGraphics,
        position: ImagePosition { row: 0, col: 0 },
        cell_size: ImageCellSize { cols: 4, rows: 2 },
        pixel_size: ImagePixelSize {
            width: 4,
            height: 2,
        },
        payload: ImagePayload {
            raw: None,
            pixels: Some(PixelBuffer {
                width: 4,
                height: 2,
                format: PixelFormat::Rgba8,
                data: (0..32).collect(),
            }),
        },
    }
}

#[cfg(feature = "kitty")]
#[test]
fn declared_working_set_preserves_late_hot_resource_under_pressure() {
    let mut state = KittyHostState::default();
    // Resource 1 is hot but painted late; resource 2 is cold. Fill the budget
    // over separate frames, as navigation would, without exceeding a frame.
    for id in 1..=128 {
        state.begin_frame();
        state.reserve_content(&mut Vec::new(), id, 32).unwrap();
        state.get_or_allocate(id).unwrap();
    }
    let hot_host = state.transmitted[&1];
    let cold_host = state.transmitted[&2];
    state.begin_frame();
    state.protect_retained(1);
    let mut output = Vec::new();
    state.reserve_content(&mut output, 129, 32).unwrap();
    state.get_or_allocate(129).unwrap();
    assert_eq!(state.transmitted.len(), 128);
    assert_eq!(state.transmitted[&1], hot_host);
    assert!(!state.transmitted.contains_key(&2));
    assert!(String::from_utf8_lossy(&output).contains(&format!("d=I,i={cold_host}")));
    state.reserve_content(&mut output, 1, 32).unwrap();
    assert_eq!(state.get_or_allocate(1).unwrap(), (hot_host, false));
    assert!(!String::from_utf8_lossy(&output).contains("a=t"));
}

#[cfg(feature = "kitty")]
#[test]
fn declared_full_working_set_rejects_overcapacity_without_eviction() {
    let mut state = KittyHostState::default();
    for id in 1..=128 {
        state.begin_frame();
        state.reserve_content(&mut Vec::new(), id, 32).unwrap();
        state.get_or_allocate(id).unwrap();
    }
    state.begin_frame();
    for id in 1..=128 {
        state.protect_retained(id);
    }
    let mut output = Vec::new();
    assert!(state.reserve_content(&mut output, 129, 32).is_err());
    assert!(output.is_empty());
    assert_eq!(state.transmitted.len(), 128);
    assert_eq!(state.content_bytes.len(), 128);
    assert!(!state.content_bytes.contains_key(&129));
}

#[cfg(feature = "kitty")]
#[test]
fn equal_content_keeps_overlapping_placement_owners_independent() {
    let mut state = KittyHostState::default();
    let first = image();
    let mut second = first.clone();
    second.id = 2;
    let output = frame(&mut state, &[first.clone(), second.clone()], &[], &kitty());
    let output = String::from_utf8(output).unwrap();
    assert_eq!(output.matches("a=t").count(), 1);
    assert_eq!(output.matches("a=p").count(), 2);
    assert_eq!(state.resources.len(), 1);
    assert_eq!(state.transmitted.len(), 2);
    assert_eq!(state.placements.len(), 2);
    let host = *state.resources.values().next().unwrap();
    let first_placement = state.placements[&(host, 0, 0, first.id)];
    let second_placement = state.placements[&(host, 0, 0, second.id)];
    assert_ne!(first_placement, second_placement);

    let output = String::from_utf8(frame(&mut state, &[second.clone()], &[], &kitty())).unwrap();
    assert!(!output.contains("a=t"));
    assert!(output.contains(&format!("a=d,d=i,i={host},p={first_placement}")));
    assert!(!output.contains(&format!("a=d,d=i,i={host},p={second_placement}")));
    assert_eq!(state.placements.len(), 1);
    assert_eq!(state.placements[&(host, 0, 0, second.id)], second_placement);

    second.payload.pixels.as_mut().unwrap().data[0] = 255;
    let output = String::from_utf8(frame(&mut state, &[first, second], &[], &kitty())).unwrap();
    assert_eq!(output.matches("a=t").count(), 1);
    assert_eq!(output.matches("a=p").count(), 2);
    assert_eq!(state.resources.len(), 2);
    assert_eq!(state.transmitted.len(), 2);
    assert_eq!(state.placements.len(), 2);
}

#[cfg(feature = "kitty")]
#[test]
fn deduplicated_residency_evicts_content_and_rebuilds_without_stale_aliases() {
    use crate::model::KittyFormat;
    let mut state = KittyHostState::default();
    let mut source = image();
    let mut first_host = 0;
    for value in 0..129_u16 {
        source.id = u64::from(value) + 1;
        state.begin_frame();
        let host = upload_kitty_data(
            &mut Vec::new(),
            &source,
            KittyFormat::Rgba,
            &value.to_le_bytes(),
            1,
            1,
            &mut state,
        )
        .unwrap();
        if value == 0 {
            first_host = host;
        }
        state.finish_frame(&mut Vec::new()).unwrap();
        assert!(state.resources.len() <= 128);
        assert!(state.content_bytes.len() <= 128);
    }
    source.id = 999;
    state.begin_frame();
    let mut output = Vec::new();
    let host = upload_kitty_data(
        &mut output,
        &source,
        KittyFormat::Rgba,
        &0_u16.to_le_bytes(),
        1,
        1,
        &mut state,
    )
    .unwrap();
    assert_ne!(host, first_host);
    assert_eq!(String::from_utf8(output).unwrap().matches("a=t").count(), 1);
    state.reset(&mut Vec::new()).unwrap();
    assert!(state.resources.is_empty());
    assert!(state.transmitted.is_empty());
    let hash = residency::Residency::fingerprint(KittyFormat::Rgba, 1, 1, &0_u16.to_le_bytes());
    assert!(
        state
            .residency
            .find(hash, KittyFormat::Rgba, 1, 1, &0_u16.to_le_bytes())
            .is_none()
    );
}

#[cfg(feature = "kitty")]
#[test]
fn alias_protection_keeps_late_shared_content_under_pressure() {
    use crate::model::KittyFormat;
    let mut state = KittyHostState::default();
    let mut source = image();
    state.begin_frame();
    let hot = upload_kitty_data(
        &mut Vec::new(),
        &source,
        KittyFormat::Rgba,
        &[0],
        1,
        1,
        &mut state,
    )
    .unwrap();
    source.id = 999;
    let alias = upload_kitty_data(
        &mut Vec::new(),
        &source,
        KittyFormat::Rgba,
        &[0],
        1,
        1,
        &mut state,
    )
    .unwrap();
    assert_eq!(hot, alias);
    for value in 1..128_u8 {
        state.begin_frame();
        source.id = u64::from(value) + 1;
        upload_kitty_data(
            &mut Vec::new(),
            &source,
            KittyFormat::Rgba,
            &[value],
            1,
            1,
            &mut state,
        )
        .unwrap();
    }
    state.begin_frame();
    state.protect_retained(999);
    source.id = 1000;
    let mut output = Vec::new();
    upload_kitty_data(
        &mut output,
        &source,
        KittyFormat::Rgba,
        &[255],
        1,
        1,
        &mut state,
    )
    .unwrap();
    assert!(state.resources.values().any(|host| *host == hot));
    source.id = 999;
    output.clear();
    let reused = upload_kitty_data(
        &mut output,
        &source,
        KittyFormat::Rgba,
        &[0],
        1,
        1,
        &mut state,
    )
    .unwrap();
    assert_eq!(reused, hot);
    assert!(output.is_empty());
    assert_eq!(state.resources.len(), 128);
}

#[cfg(feature = "kitty")]
#[test]
fn alias_churn_preserves_canonical_seen_and_predeclared_protection() {
    use crate::model::KittyFormat;
    let mut state = KittyHostState::default();
    let mut source = image();
    for value in 0..128_u8 {
        state.begin_frame();
        source.id = u64::from(value) + 1;
        upload_kitty_data(
            &mut Vec::new(),
            &source,
            KittyFormat::Rgba,
            &[value],
            1,
            1,
            &mut state,
        )
        .unwrap();
    }
    let hot = state.transmitted[&1];
    state.begin_frame();
    state.protect_retained(1);
    assert!(state.frame_protected.contains(&1));
    for owner in 1000..2025 {
        source.id = owner;
        let reused = upload_kitty_data(
            &mut Vec::new(),
            &source,
            KittyFormat::Rgba,
            &[1],
            1,
            1,
            &mut state,
        )
        .unwrap();
        assert_eq!(reused, state.resources[&2]);
        assert!(state.frame_protected.contains(&1));
        assert!(state.frame_seen.contains(&2));
    }
    // Owner 1 is no longer a bounded alias hint, but the frame declaration is
    // already canonical and cannot be undone by alias eviction.
    assert!(!state.residency.has_owner(1));
    source.id = 3000;
    let mut output = Vec::new();
    upload_kitty_data(
        &mut output,
        &source,
        KittyFormat::Rgba,
        &[255],
        1,
        1,
        &mut state,
    )
    .unwrap();
    assert_eq!(state.resources[&1], hot);
    assert!(state.resources.contains_key(&2));
    assert_eq!(state.resources.len(), 128);
    assert_eq!(state.frame_seen.len(), 2);
    assert_eq!(state.residency.retained_bytes(), 128);
    // Each one-byte equality buffer plus conservative 4-byte raster cost.
    assert_eq!(state.content_bytes.values().sum::<usize>(), 128 * 5);
}

#[cfg(feature = "kitty")]
#[test]
fn equality_storage_is_charged_before_upload_or_allocation() {
    let mut state = KittyHostState::default();
    let mut output = Vec::new();
    // The conservative raster alone fills 64 MiB; even a one-byte equality
    // copy cannot be admitted on top. No equality allocation or upload occurs.
    assert!(
        upload_kitty_data(
            &mut output,
            &image(),
            crate::model::KittyFormat::Png,
            &[0],
            4096,
            4096,
            &mut state,
        )
        .is_err()
    );
    assert!(output.is_empty());
    assert!(state.resources.is_empty());
    assert!(state.content_bytes.is_empty());
    assert_eq!(state.residency.retained_bytes(), 0);
}

#[cfg(feature = "kitty")]
#[test]
fn same_owner_replacement_does_not_retarget_an_earlier_placement() {
    use crate::model::KittyFormat;
    let mut state = KittyHostState::default();
    let source = image();
    state.begin_frame();
    let old = upload_kitty_data(
        &mut Vec::new(),
        &source,
        KittyFormat::Rgba,
        &[0; 4],
        1,
        1,
        &mut state,
    )
    .unwrap();
    state
        .place(&mut Vec::new(), (old, source.id), 0, 0, 1, 1)
        .unwrap();
    let old_placement = state.placements[&(old, 0, 0, source.id)];
    let mut output = Vec::new();
    let new = upload_kitty_data(
        &mut output,
        &source,
        KittyFormat::Rgba,
        &[1; 4],
        1,
        1,
        &mut state,
    )
    .unwrap();
    state
        .place(&mut output, (new, source.id), 0, 0, 1, 1)
        .unwrap();
    state.finish_frame(&mut output).unwrap();
    assert_ne!(old, new);
    assert_eq!(state.placements[&(old, 0, 0, source.id)], old_placement);
    assert!(state.placements.contains_key(&(new, 0, 0, source.id)));
    assert_eq!(state.frame_seen.len(), 2);
    assert_eq!(state.residency.retained_bytes(), 8);
    assert_eq!(state.content_bytes.values().sum::<usize>(), 16);
    let output = String::from_utf8(output).unwrap();
    assert!(!output.contains("a=d"));
}

#[cfg(feature = "kitty")]
#[test]
fn content_identity_checks_dimensions_and_format_and_repairs_failed_candidate() {
    use crate::model::KittyFormat;
    let mut committed = KittyHostState::default();
    let source = image();
    let data = [0; 16];
    committed.begin_frame();
    let first = upload_kitty_data(
        &mut Vec::new(),
        &source,
        KittyFormat::Rgba,
        &data,
        2,
        2,
        &mut committed,
    )
    .unwrap();
    let mut candidate = committed.clone();
    let mut output = Vec::new();
    let changed_dimensions = upload_kitty_data(
        &mut output,
        &source,
        KittyFormat::Rgba,
        &data,
        1,
        4,
        &mut candidate,
    )
    .unwrap();
    let changed_format = upload_kitty_data(
        &mut output,
        &source,
        KittyFormat::Rgb,
        &data,
        2,
        2,
        &mut candidate,
    )
    .unwrap();
    assert_ne!(first, changed_dimensions);
    assert_ne!(first, changed_format);
    assert_ne!(changed_dimensions, changed_format);
    // Model a flush failure: the candidate has seen bytes but is not committed.
    assert_eq!(committed.transmitted.len(), 1);
    committed.invalidate_after_failed_output(&candidate);
    let mut retry = Vec::new();
    committed.begin_frame();
    let repaired = upload_kitty_data(
        &mut retry,
        &source,
        KittyFormat::Rgba,
        &data,
        2,
        2,
        &mut committed,
    )
    .unwrap();
    assert_ne!(repaired, first);
    let retry = String::from_utf8(retry).unwrap();
    assert!(retry.find("d=I").unwrap() < retry.find("a=t").unwrap());
    assert_eq!(committed.transmitted.len(), 1);
    let mut warm = Vec::new();
    upload_kitty_data(
        &mut warm,
        &source,
        KittyFormat::Rgba,
        &data,
        2,
        2,
        &mut committed,
    )
    .unwrap();
    assert!(warm.is_empty());
}

fn pane() -> PaneRect {
    PaneRect {
        x: 0,
        y: 0,
        w: 8,
        h: 8,
    }
}
#[cfg(feature = "kitty")]
fn kitty() -> HostImageCapabilities {
    HostImageCapabilities {
        kitty_graphics: true,
        cell_pixel_width: 1,
        cell_pixel_height: 1,
        ..HostImageCapabilities::default()
    }
}
#[cfg(feature = "kitty")]
#[test]
fn kitty_placements_position_cursor_for_nonzero_cached_and_cropped_output() {
    let mut state = KittyHostState::default();
    state.begin_frame();
    let mut output = Vec::new();
    state.place(&mut output, (42, 42), 17, 9, 4, 2).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.starts_with("\x1b[10;18H\x1b_Ga=p,"), "{output:?}");

    // Cached placements must not depend on the cursor left by text painting.
    state.begin_frame();
    let mut output = b"\x1b[1;1H".to_vec();
    state.place(&mut output, (42, 42), 17, 9, 4, 2).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("\x1b[10;18H\x1b_Ga=p,"), "{output:?}");
    assert!(!output.contains("a=t"));

    let mut output = Vec::new();
    state
        .place_cropped(
            &mut output,
            (43, 43),
            (23, 11),
            (3, 2),
            Some(crate::model::KittySourceRect {
                x: 8,
                y: 16,
                width: 24,
                height: 32,
            }),
        )
        .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.starts_with("\x1b[12;24H\x1b_Ga=p,"), "{output:?}");
    assert!(output.contains("x=8,y=16,w=24,h=32"), "{output:?}");
}

#[cfg(feature = "kitty")]
#[test]
fn compositor_hide_reveal_roundtrips_through_nested_registry() {
    use crate::codec::kitty::parse_command;
    use crate::model::{ImageEvent, ImagePosition};
    use crate::registry::ImageRegistry;
    let mut host = KittyHostState::default();
    let mut nested = ImageRegistry::default();
    let mut apply = |output: &[u8]| {
        for body in output.split(|byte| *byte == 0x1b) {
            if let Some(body) = body.strip_prefix(b"_") {
                let command = parse_command(body, ImagePosition { row: 0, col: 0 }).unwrap();
                nested.handle_event(
                    ImageEvent::KittyCommand {
                        command,
                        filtered_byte_offset: 0,
                    },
                    1,
                    1,
                );
            }
        }
        nested.images().len()
    };
    let first = frame(&mut host, &[image()], &[], &kitty());
    assert_eq!(apply(&first), 1);
    let mut hidden = Vec::new();
    host.begin_frame();
    host.clear_placements(&mut hidden).unwrap();
    host.finish_frame(&mut hidden).unwrap();
    assert_eq!(apply(&hidden), 0);
    let revealed = frame(&mut host, &[image()], &[], &kitty());
    assert!(!String::from_utf8_lossy(&revealed).contains("a=t"));
    assert_eq!(apply(&revealed), 1);
}

fn frame(
    state: &mut KittyHostState,
    images: &[PaneImage],
    covers: &[bmux_tui::geometry::Rect],
    caps: &HostImageCapabilities,
) -> Vec<u8> {
    let mut output = Vec::new();
    state.begin_frame();
    render_pane_images_clipped(&mut output, images, pane(), covers, caps, state).unwrap();
    state.finish_frame(&mut output).unwrap();
    output
}

#[cfg(feature = "kitty")]
#[test]
fn movement_hide_reveal_and_changed_pixels_reconcile_content() {
    let mut state = KittyHostState::default();
    let mut image = image();
    assert!(
        String::from_utf8(frame(&mut state, &[image.clone()], &[], &kitty()))
            .unwrap()
            .contains("a=t")
    );
    assert_eq!(state.work_counts(), (1, 1));
    image.position.col = 2;
    let moved = String::from_utf8(frame(&mut state, &[image.clone()], &[], &kitty())).unwrap();
    assert!(!moved.contains("a=t"));
    assert!(moved.contains("a=d,d=i"));
    assert_eq!(state.work_counts(), (1, 1));
    assert!(
        String::from_utf8(frame(&mut state, &[], &[], &kitty()))
            .unwrap()
            .contains("a=d,d=i")
    );
    assert!(
        !String::from_utf8(frame(&mut state, &[image.clone()], &[], &kitty()))
            .unwrap()
            .contains("a=t")
    );
    image.payload.pixels.as_mut().unwrap().data[0] = 255;
    assert!(
        String::from_utf8(frame(&mut state, &[image], &[], &kitty()))
            .unwrap()
            .contains("a=t")
    );
    assert_eq!(state.work_counts(), (2, 2));
}

#[cfg(feature = "kitty")]
#[test]
fn crop_fragments_are_stable_and_overlay_reveal_reuses_source() {
    let mut state = KittyHostState::default();
    let covers = [bmux_tui::geometry::Rect::new(1, 0, 2, 1)];
    frame(&mut state, &[image()], &covers, &kitty());
    let counts = state.work_counts();
    assert_eq!(counts.0, 1);
    assert_eq!(counts.1, 3);
    let output = String::from_utf8(frame(&mut state, &[image()], &covers, &kitty())).unwrap();
    assert!(!output.contains("a=t"));
    assert_eq!(state.work_counts(), counts);
    frame(&mut state, &[image()], &[], &kitty());
    assert_eq!(state.work_counts(), (1, 4));
}

#[cfg(feature = "kitty")]
#[test]
fn failed_candidate_tracks_ids_even_after_eviction_and_repairs_before_transmit() {
    let mut committed = KittyHostState::default();
    let mut candidate = committed.clone();
    frame(&mut candidate, &[image()], &[], &kitty());
    let allocated = candidate.allocated_ids.clone();
    candidate.transmitted.clear(); // Model eviction after partial output.
    committed.invalidate_after_failed_output(&candidate);
    assert!(allocated.iter().all(|id| committed.repair_ids.contains(id)));
    let output = String::from_utf8(frame(&mut committed, &[image()], &[], &kitty())).unwrap();
    assert!(output.find("d=I").unwrap() < output.find("a=t").unwrap());
    assert!(
        committed
            .transmitted
            .values()
            .all(|id| !allocated.contains(id))
    );
}

#[cfg(feature = "kitty")]
#[test]
fn partial_writer_candidate_never_advances_committed_cache() {
    struct Fails;
    impl Write for Fails {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected write failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut committed = KittyHostState::default();
    let mut candidate = committed.clone();
    candidate.begin_frame();
    let source = image();
    let pixels = source.payload.pixels.as_ref().unwrap();
    assert!(
        emit_kitty_data(
            &mut Fails,
            &source,
            crate::model::KittyFormat::Rgba,
            &pixels.data,
            pixels.width,
            pixels.height,
            0,
            0,
            &mut candidate
        )
        .is_err()
    );
    committed.invalidate_after_failed_output(&candidate);
    assert_eq!(committed.work_counts(), (0, 0));
    assert!(committed.transmitted.is_empty());
    assert!(!committed.repair_ids.is_empty());
    frame(&mut committed, &[image()], &[], &kitty());
}

#[cfg(feature = "kitty")]
#[test]
fn inactive_host_content_and_cpu_cache_are_bounded() {
    let mut state = KittyHostState::default();
    for value in 0..150_u8 {
        let mut source = image();
        source.payload.pixels.as_mut().unwrap().data[0] = value;
        frame(&mut state, &[source], &[], &kitty());
    }
    assert!(state.transmitted.len() <= 128);
    assert!(state.content_bytes.values().sum::<usize>() <= 64 * 1024 * 1024);
    let mut output = Vec::new();
    state.reset(&mut output).unwrap();
    assert!(state.transmitted.is_empty());
    assert!(String::from_utf8(output).unwrap().contains("d=I"));
}

#[cfg(feature = "sixel")]
#[test]
fn sixel_scaling_and_encoding_are_reused_but_cell_metrics_invalidate() {
    let mut state = KittyHostState::default();
    let mut caps = HostImageCapabilities {
        sixel: true,
        cell_pixel_width: 2,
        cell_pixel_height: 2,
        ..HostImageCapabilities::default()
    };
    let first = frame(&mut state, &[image()], &[], &caps);
    assert_eq!(first, frame(&mut state, &[image()], &[], &caps));
    assert_eq!(state.work_counts(), (1, 1));
    caps.cell_pixel_width = 3;
    frame(&mut state, &[image()], &[], &caps);
    assert_eq!(state.work_counts(), (1, 2));
}

#[cfg(feature = "kitty")]
#[test]
fn byte_budget_evicts_inactive_content_before_upload_and_reset_releases_it() {
    let mut state = KittyHostState::default();
    state.begin_frame();
    let (old, _) = state.get_or_allocate(1).unwrap();
    state.content_bytes.insert(1, 48 * 1024 * 1024);
    state.begin_frame();
    let mut output = Vec::new();
    state
        .reserve_content(&mut output, 2, 32 * 1024 * 1024)
        .unwrap();
    assert!(!state.transmitted.contains_key(&1));
    assert_eq!(
        state.content_bytes.values().sum::<usize>(),
        32 * 1024 * 1024
    );
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains(&format!("d=I,i={old}"))
    );
    state.get_or_allocate(2).unwrap();
    state.reset(&mut Vec::new()).unwrap();
    assert!(state.transmitted.is_empty());
    assert!(state.content_bytes.is_empty());
}

#[cfg(feature = "kitty")]
#[test]
fn protocol_switch_releases_hidden_content_and_retransmits_on_return() {
    let mut state = KittyHostState::default();
    frame(&mut state, &[image()], &[], &kitty());
    frame(&mut state, &[], &[], &kitty());
    let output = frame(&mut state, &[], &[], &HostImageCapabilities::default());
    assert!(String::from_utf8(output).unwrap().contains("d=I"));
    assert!(state.transmitted.is_empty());
    assert!(state.content_bytes.is_empty());
    assert!(
        String::from_utf8(frame(&mut state, &[image()], &[], &kitty()))
            .unwrap()
            .contains("a=t")
    );
}

#[cfg(feature = "kitty")]
#[test]
fn validated_png_is_preserved_only_for_full_source() {
    use image::ImageEncoder;
    let mut source = image();
    let pixels = source.payload.pixels.take().unwrap();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &pixels.data,
            pixels.width,
            pixels.height,
            image::ColorType::Rgba8.into(),
        )
        .unwrap();
    source.payload.pixels = Some(PixelBuffer {
        data: png.clone(),
        format: PixelFormat::Png,
        ..pixels
    });
    let mut state = KittyHostState::default();
    let full = String::from_utf8(frame(&mut state, &[source.clone()], &[], &kitty())).unwrap();
    assert!(full.contains("f=100"));
    assert!(full.contains(&crate::codec::kitty::base64_encode(&png)));
    let clipped = String::from_utf8(frame(
        &mut state,
        &[source],
        &[bmux_tui::geometry::Rect::new(0, 0, 2, 2)],
        &kitty(),
    ))
    .unwrap();
    assert!(clipped.contains("f=32"));
    assert!(!clipped.contains("f=100"));
    assert_eq!(state.work_counts(), (1, 2));
}

#[cfg(feature = "kitty")]
fn native_png_work_source(side: u32) -> PaneImage {
    use image::ImageEncoder;
    // Incompressible, deterministic valid PNG for payload-size-dependent work.
    let mut random = 1_u32;
    let rgba: Vec<u8> = (0..side * side)
        .flat_map(|_| {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            random.to_le_bytes()
        })
        .collect();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&rgba, side, side, image::ColorType::Rgba8.into())
        .unwrap();
    let mut source = image();
    source.id = 71;
    source.position = ImagePosition { row: 17, col: 19 };
    source.pixel_size = ImagePixelSize {
        width: side,
        height: side,
    };
    source.payload = ImagePayload {
        raw: Some(vec![9; 37]),
        pixels: Some(PixelBuffer {
            width: side,
            height: side,
            format: PixelFormat::Png,
            data: png,
        }),
    };
    source
}

#[cfg(feature = "kitty")]
#[test]
fn frozen_native_png_matches_checked_wire_and_skips_only_warm_scans() {
    use bmux_tui::geometry::Rect;
    use bmux_tui::image::{ImageFit, ImageKey, ImageLifecycle, ImagePlacement};

    let source = native_png_work_source(16);
    let frozen = crate::frozen::FrozenImage::new(source.clone());
    let pixels = source.payload.pixels.as_ref().unwrap();
    let mut placement = ImagePlacement {
        key: ImageKey::new("frozen-wire-reference"),
        payload: bmux_tui::image::ImagePayload::Png {
            bytes: pixels.data.clone(),
            width: 16,
            height: 16,
        },
        destination: Rect::new(3, 5, 32, 40),
        allocation: None,
        clip: Rect::new(0, 0, 100, 100),
        fit: ImageFit::Contain,
        lifecycle: ImageLifecycle::Frame,
    };
    let mut checked = KittyHostState::default();
    let mut owned = KittyHostState::default();
    for frame in 0..4 {
        placement.destination.x += 1;
        checked.begin_frame();
        owned.begin_frame();
        let mut expected = Vec::new();
        assert!(
            checked
                .contain_png(&mut expected, &source, &placement, &kitty(), placement.clip)
                .unwrap()
        );
        checked.finish_frame(&mut expected).unwrap();
        crate::model::test_work::take();
        let mut actual = Vec::new();
        assert!(
            owned
                .contain_frozen_png(&mut actual, &frozen, &placement, &kitty(), placement.clip)
                .unwrap()
        );
        owned.finish_frame(&mut actual).unwrap();
        let work = crate::model::test_work::take();
        assert_eq!(actual, expected);
        assert_eq!(work.clone_bytes, 0);
        if frame > 0 {
            assert_eq!(work.hash_bytes, 0);
            assert_eq!(work.equality_bytes, 0);
        }
    }
}

#[cfg(feature = "kitty")]
#[test]
fn native_png_warm_placement_scans_content_without_cloning_payload() {
    use bmux_tui::geometry::Rect;
    use bmux_tui::image::{ImageFit, ImageKey, ImageLifecycle, ImagePlacement};

    for side in [16_u32, 512] {
        let source = native_png_work_source(side);
        let original = source.clone();
        let pixels = source.payload.pixels.as_ref().unwrap();
        let placement = ImagePlacement {
            key: ImageKey::new("native-png-work"),
            payload: bmux_tui::image::ImagePayload::Png {
                bytes: pixels.data.clone(),
                width: side,
                height: side,
            },
            destination: Rect::new(3, 5, 32, 40),
            allocation: None,
            clip: Rect::new(0, 0, 100, 100),
            fit: ImageFit::Contain,
            lifecycle: ImageLifecycle::Frame,
        };
        let mut state = KittyHostState::default();
        state.begin_frame();
        assert!(
            state
                .contain_png(
                    &mut Vec::new(),
                    &source,
                    &placement,
                    &kitty(),
                    placement.clip
                )
                .unwrap()
        );
        state.finish_frame(&mut Vec::new()).unwrap();
        let host = state.transmitted[&source.id];
        let retained = state.residency.retained_bytes();
        let mut reference = state.clone();
        reference.begin_frame();
        crate::model::test_work::take();
        // Reproduce the old clone-and-override operation and wire output. All
        // source metadata survives except the intended fitted cell dimensions.
        let fragment = PaneImage {
            cell_size: ImageCellSize { cols: 32, rows: 0 },
            ..source.clone()
        };
        let mut expected = b"\x1b[10;4H".to_vec();
        emit_kitty_data(
            &mut expected,
            &fragment,
            crate::model::KittyFormat::Png,
            &pixels.data,
            side,
            side,
            3,
            9,
            &mut reference,
        )
        .unwrap();
        reference.finish_frame(&mut expected).unwrap();
        let old_work = crate::model::test_work::take();
        assert_eq!(old_work.clone_bytes, pixels.data.len() + 37);
        assert_eq!(old_work.payload_clones, 2);

        for _ in 0..3 {
            state.begin_frame();
            crate::model::test_work::take();
            let mut output = Vec::new();
            assert!(
                state
                    .contain_png(&mut output, &source, &placement, &kitty(), placement.clip)
                    .unwrap()
            );
            state.finish_frame(&mut output).unwrap();
            let work = crate::model::test_work::take();
            assert_eq!(work.clone_bytes, 0);
            assert_eq!(work.payload_clones, 0);
            // Deliberately document remaining canonical scans. No speculative
            // owner/pointer identity shortcut replaces collision-proof equality.
            assert_eq!(work.hash_bytes, pixels.data.len());
            assert_eq!(work.equality_bytes, pixels.data.len());
            assert_eq!(output, expected);
            assert!(!String::from_utf8_lossy(&output).contains("a=t"));
            assert_eq!(state.transmitted[&source.id], host);
            assert_eq!(state.residency.retained_bytes(), retained);
            assert_eq!(source, original);
        }
    }
}

#[cfg(feature = "kitty")]
#[test]
#[ignore = "opt-in image composition workload; reports work, no wall-clock gates"]
fn benchmark_repeated_clipped_composition() {
    let mut state = KittyHostState::default();
    let source = image();
    let covers = [bmux_tui::geometry::Rect::new(1, 0, 2, 1)];
    for _ in 0..10_000 {
        frame(&mut state, std::slice::from_ref(&source), &covers, &kitty());
    }
    eprintln!(
        "decode/prepare work after 10,000 frames: {:?}",
        state.work_counts()
    );
    assert_eq!(state.work_counts(), (1, 3));
}
