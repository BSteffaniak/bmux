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
    assert!(moved.contains("a=d,d=p"));
    assert_eq!(state.work_counts(), (1, 1));
    assert!(
        String::from_utf8(frame(&mut state, &[], &[], &kitty()))
            .unwrap()
            .contains("a=d,d=p")
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
