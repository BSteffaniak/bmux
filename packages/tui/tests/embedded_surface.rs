use std::sync::Arc;

// Reuse the executable's caller-owned producer and dashboard, not a separate
// test-only renderer or transport implementation.
#[path = "../examples/embedded_surface.rs"]
mod live_dashboard;

use bmux_tui::{
    buffer::Buffer,
    component::{Component, Constraints, LayoutCx},
    damage::Damage,
    embedded_surface::{
        EmbeddedSurface, RetainedSurface, SurfaceError, SurfaceIdentity, SurfaceLimits,
    },
    geometry::{Point, Rect, Size},
    image::{
        ImageContribution, ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat,
        ImagePlacement,
    },
    image_scene::ImageScene,
    presentation::{PresentationLimits, PresentationState},
    style::Style,
};

fn identity(revision: u64) -> SurfaceIdentity {
    SurfaceIdentity {
        owner: "sensor/front-room".into(),
        generation: 4,
        revision,
    }
}

fn limits() -> SurfaceLimits {
    SurfaceLimits {
        max_cells: 100,
        max_bytes: 8192,
    }
}

fn surface(revision: u64) -> RetainedSurface {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
    buffer.set_cell(Point::new(0, 0), "界", Style::new());
    buffer.set_cell(Point::new(2, 0), "e\u{301}", Style::new());
    RetainedSurface::new(
        identity(revision),
        Arc::new(buffer),
        Arc::default(),
        limits(),
    )
    .unwrap()
}

fn layout(surface: &RetainedSurface, width: u16, height: u16) -> bmux_tui::component::LayoutNode {
    EmbeddedSurface::new(surface).layout(
        Constraints::tight(Size::new(width, height)),
        &mut LayoutCx::default(),
    )
}

#[test]
fn snapshot_clones_share_native_storage_and_revision_tagged_layout() {
    let source = surface(9);
    let copy = source.clone();
    assert!(Arc::ptr_eq(source.buffer(), copy.buffer()));
    assert!(Arc::ptr_eq(source.images(), copy.images()));
    let committed = layout(&source, 8, 3);
    let component = EmbeddedSurface::new(&copy);
    let point = component.input_point(&committed, 2, 1).unwrap();
    assert_eq!((point.x, point.y), (0, 0));
    assert_eq!(point.identity, identity(9));
    assert!(component.input_point(&committed, 1, 1).is_none());
    assert!(component.input_point(&committed, 6, 1).is_none());
    assert!(
        EmbeddedSurface::new(&surface(10))
            .placement(&committed)
            .is_none()
    );
    let mut changed_generation = identity(9);
    changed_generation.generation += 1;
    let replacement = RetainedSurface::new(
        changed_generation,
        Arc::clone(source.buffer()),
        Arc::clone(source.images()),
        limits(),
    )
    .unwrap();
    assert!(
        EmbeddedSurface::new(&replacement)
            .placement(&committed)
            .is_none()
    );
    assert_ne!(
        component.revision(),
        EmbeddedSurface::new(&replacement).revision()
    );
    let mut other_owner = identity(9);
    other_owner.owner = "sensor/back-room".into();
    let other = RetainedSurface::new(
        other_owner,
        Arc::clone(source.buffer()),
        Arc::clone(source.images()),
        limits(),
    )
    .unwrap();
    assert_ne!(
        component.revision(),
        EmbeddedSurface::new(&other).revision()
    );
    assert_eq!(
        component.revision(),
        EmbeddedSurface::new(&source).revision()
    );
}

#[test]
fn wide_glyph_clipping_and_input_use_the_same_resolved_viewport() {
    let source = surface(9);
    let component = EmbeddedSurface::new(&source);
    let committed = layout(&source, 2, 1);
    let state = PresentationState::new(Rect::new(0, 0, 2, 1));
    let prepared = state
        .prepare(Damage::Full, |cx| component.paint(&committed, cx))
        .unwrap();
    // Central clipping begins on the wide continuation. The painter must not
    // expose half a wide glyph, but must retain the complete combining grapheme.
    assert_eq!(prepared.buffer().cells()[0].symbol, " ");
    assert_eq!(prepared.buffer().cells()[1].symbol, "e\u{301}");
    let point = component.input_point(&committed, 0, 0).unwrap();
    assert_eq!((point.x, point.y), (1, 0));
    assert!(prepared.hits().regions().is_empty());
}

#[test]
fn failed_resize_output_does_not_publish_pending_layout() {
    let source = surface(9);
    let component = EmbeddedSurface::new(&source);
    let committed = layout(&source, 8, 3);
    let pending = layout(&source, 2, 1);
    let mut state = PresentationState::new(Rect::new(0, 0, 8, 3));
    state
        .present(
            Damage::Full,
            |cx| component.paint(&committed, cx),
            |_, _| Ok::<(), ()>(()),
        )
        .unwrap();
    let staged = state.prepare(Damage::Full, |cx| component.paint(&pending, cx));
    assert!(
        state
            .present_prepared(staged, |_, _| Err::<(), _>("backend failed"))
            .is_err()
    );
    // The application keeps the previously committed layout after failure.
    assert_eq!(component.input_point(&committed, 2, 1).unwrap().x, 0);
    assert!(component.input_point(&pending, 2, 1).is_none());
}

fn image(payload: ImagePayload) -> ImageScene {
    let mut scene = ImageScene::default();
    scene.reconcile(&[ImageContribution::Present(ImagePlacement {
        key: ImageKey::new("sensor-icon"),
        payload,
        destination: Rect::new(0, 0, 2, 1),
        clip: Rect::new(0, 0, 2, 1),
        fit: ImageFit::Contain,
        allocation: None,
        lifecycle: ImageLifecycle::Persistent,
    })]);
    scene
}

#[test]
fn native_image_validation_bounds_payloads_and_decoded_resources() {
    let buffer = Arc::new(Buffer::empty(Rect::new(0, 0, 2, 1)));
    for (payload, expected) in [
        (
            ImagePayload::Pixels {
                bytes: vec![0; 3],
                width: 1,
                height: 1,
                format: ImagePixelFormat::Rgba8,
            },
            SurfaceError::Invalid,
        ),
        (
            ImagePayload::Pixels {
                bytes: vec![],
                width: 4097,
                height: 1,
                format: ImagePixelFormat::Rgb8,
            },
            SurfaceError::Limit,
        ),
        (
            ImagePayload::Png {
                bytes: vec![0; 10],
                width: 1,
                height: 1,
            },
            SurfaceError::Invalid,
        ),
    ] {
        let result = RetainedSurface::new(
            identity(1),
            Arc::clone(&buffer),
            Arc::new(image(payload)),
            limits(),
        );
        assert_eq!(result.unwrap_err(), expected);
    }
    let result = RetainedSurface::new(
        identity(1),
        Arc::clone(&buffer),
        Arc::default(),
        SurfaceLimits {
            max_cells: 1,
            max_bytes: 8192,
        },
    );
    assert_eq!(result.unwrap_err(), SurfaceError::Limit);
    let result = RetainedSurface::new(
        identity(1),
        buffer,
        Arc::default(),
        SurfaceLimits {
            max_cells: 2,
            max_bytes: 1,
        },
    );
    assert_eq!(result.unwrap_err(), SurfaceError::Limit);
}

#[test]
fn native_cell_ingestion_rejects_mutated_graphemes_and_controls() {
    for symbol in ["ab", "\u{1b}", "\u{301}"] {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        // Native cell symbols are public; ingestion still validates them instead
        // of assuming every producer used the safe painting setters.
        buffer.get_mut(Point::new(0, 0)).unwrap().symbol = symbol.into();
        let result = RetainedSurface::new(identity(1), Arc::new(buffer), Arc::default(), limits());
        assert_eq!(result.unwrap_err(), SurfaceError::Invalid);
    }
}

#[test]
fn valid_png_is_admitted_once_and_paints_from_the_retained_scene() {
    use image::ImageEncoder;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[10, 20, 30, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .unwrap();
    let source = RetainedSurface::new(
        identity(1),
        Arc::new(Buffer::empty(Rect::new(0, 0, 2, 1))),
        Arc::new(image(ImagePayload::Png {
            bytes,
            width: 1,
            height: 1,
        })),
        limits(),
    )
    .unwrap();
    let component = EmbeddedSurface::new(&source);
    let committed = layout(&source, 2, 1);
    let state = PresentationState::new(Rect::new(0, 0, 2, 1));
    // Component painting has no decode path; all input has already been admitted.
    let prepared = state
        .prepare(Damage::Full, |cx| component.paint(&committed, cx))
        .unwrap();
    assert_eq!(prepared.image_scene().placements().len(), 1);
}

#[test]
fn persistent_source_images_are_frame_scoped_in_the_embedding() {
    let images = Arc::new(image(ImagePayload::Pixels {
        bytes: vec![0; 4],
        width: 1,
        height: 1,
        format: ImagePixelFormat::Rgba8,
    }));
    let source = RetainedSurface::new(
        identity(1),
        Arc::new(Buffer::empty(Rect::new(0, 0, 2, 1))),
        Arc::clone(&images),
        limits(),
    )
    .unwrap();
    assert!(Arc::ptr_eq(source.images(), &images));
    let component = EmbeddedSurface::new(&source);
    let committed = layout(&source, 6, 3);
    let mut state = PresentationState::new(Rect::new(0, 0, 6, 3));
    let output_limits = PresentationLimits {
        max_cells: 18,
        max_entries: 16,
        max_bytes: 8192,
    };
    state
        .present_bounded(
            output_limits,
            Damage::Full,
            |cx| component.paint(&committed, cx),
            |prepared, _| {
                let placement = prepared.image_scene().placements().next().unwrap();
                assert_eq!(placement.destination, Rect::new(2, 1, 2, 1));
                assert_eq!(placement.lifecycle, ImageLifecycle::Frame);
                Ok::<(), ()>(())
            },
        )
        .unwrap();
    state
        .present_bounded(
            output_limits,
            Damage::Full,
            |_| {},
            |prepared, _| {
                assert_eq!(prepared.image_scene().placements().len(), 0);
                Ok::<(), ()>(())
            },
        )
        .unwrap();
}

#[test]
fn live_producer_advances_without_paint_and_slow_reader_gets_latest_full_repair() {
    use std::time::Duration;
    let mut producer = live_dashboard::Producer::start(Duration::from_millis(2));
    let first = producer.latest_at_least(1).unwrap();
    let first_revision = first.identity().revision;
    // No consumer paint, acknowledgment, or producer step call occurs here.
    let repaired = producer.latest_at_least(first_revision + 5).unwrap();
    assert!(repaired.identity().revision >= first_revision + 5);
    assert_eq!(repaired.identity().owner, first.identity().owner);
    assert_eq!(repaired.identity().generation, first.identity().generation);
    assert_eq!(repaired.buffer().area(), Rect::new(0, 0, 28, 3));
    assert!(
        repaired
            .buffer()
            .row_symbols(1)
            .unwrap()
            .contains(&format!("sample {}", repaired.identity().revision))
    );
    let (published, coalesced) = producer.counts();
    assert!(published >= repaired.identity().revision);
    assert!(coalesced >= 4);
    // Acquired snapshots stay immutable after the producer has advanced.
    assert!(
        first
            .buffer()
            .row_symbols(1)
            .unwrap()
            .contains(&format!("sample {first_revision}"))
    );
    let mut dashboard = live_dashboard::Dashboard::new(std::io::sink());
    dashboard.present(&first).unwrap();
    assert!(dashboard.accepts_input(first.identity()));
    dashboard.present(&repaired).unwrap();
    assert_eq!(dashboard.committed_identity(), Some(repaired.identity()));
    assert!(!dashboard.accepts_input(first.identity()));
    assert!(dashboard.accepts_input(repaired.identity()));
    // A delayed callback cannot roll the displayed identity back.
    assert!(dashboard.present(&first).is_err());
    assert_eq!(dashboard.committed_identity(), Some(repaired.identity()));
    producer.stop().unwrap();
    let stopped_counts = producer.counts();
    assert!(producer.latest_at_least(published + 1).is_err());
    std::thread::sleep(Duration::from_millis(8));
    assert_eq!(producer.counts(), stopped_counts);
    producer.stop().unwrap(); // owned join/cancel is idempotent
}

#[test]
fn live_dashboard_failed_flush_keeps_commit_then_repairs_and_rejects_foreign_identity() {
    use std::{
        io::{self, Write},
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };
    struct FailingWriter(Arc<AtomicBool>);
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.0.load(Ordering::SeqCst) {
                Err(io::Error::other("injected output failure"))
            } else {
                Ok(())
            }
        }
    }
    let mut producer = live_dashboard::Producer::start(Duration::from_millis(2));
    let first = producer.latest_at_least(1).unwrap();
    let fail = Arc::new(AtomicBool::new(false));
    let mut dashboard = live_dashboard::Dashboard::new(FailingWriter(Arc::clone(&fail)));
    dashboard.present(&first).unwrap();
    let next = producer
        .latest_at_least(first.identity().revision + 3)
        .unwrap();
    fail.store(true, Ordering::SeqCst);
    assert!(dashboard.present(&next).is_err());
    assert_eq!(dashboard.committed_identity(), Some(first.identity()));
    assert!(dashboard.accepts_input(first.identity()));
    assert!(!dashboard.accepts_input(next.identity()));
    // Acquisition continues even while the consumer's output is failing.
    let repaired = producer
        .latest_at_least(next.identity().revision + 3)
        .unwrap();
    fail.store(false, Ordering::SeqCst);
    assert!(dashboard.present(&repaired).unwrap().full_repaint);
    assert_eq!(dashboard.committed_identity(), Some(repaired.identity()));
    assert!(!dashboard.accepts_input(next.identity()));
    assert!(dashboard.present(&next).is_err());
    assert!(dashboard.present(&repaired).is_err()); // duplicate revision
    for changed_owner in [false, true] {
        let mut foreign = repaired.identity().clone();
        foreign.revision += 1;
        if changed_owner {
            foreign.owner = "sensor/other".into();
        } else {
            foreign.generation += 1;
        }
        let foreign = RetainedSurface::new(
            foreign,
            Arc::clone(repaired.buffer()),
            Arc::clone(repaired.images()),
            SurfaceLimits {
                max_cells: 84,
                max_bytes: 8192,
            },
        )
        .unwrap();
        assert!(dashboard.present(&foreign).is_err());
        assert!(!dashboard.accepts_input(foreign.identity()));
    }
    assert_eq!(dashboard.committed_identity(), Some(repaired.identity()));
    producer.stop().unwrap();
}

#[test]
fn cancellation_drop_joins_even_a_long_interval_sleeper() {
    use std::{sync::atomic::Ordering, time::Duration};
    let producer = live_dashboard::Producer::start(Duration::from_secs(60));
    producer.latest_at_least(1).unwrap();
    let joined = producer.cleanup_probe();
    assert!(!joined.load(Ordering::SeqCst));
    drop(producer); // notification interrupts its sixty-second sample interval
    assert!(joined.load(Ordering::SeqCst));
}

#[test]
fn finite_headless_acceptance_uses_real_terminal_commit_path() {
    let (published, coalesced) = live_dashboard::run(std::io::sink(), 3).unwrap();
    assert!(published >= 3);
    assert!(coalesced > 0);
}

#[test]
fn output_failure_cancels_owned_producer_on_early_return() {
    use std::io::{self, Write};
    struct FailedOutput;
    impl Write for FailedOutput {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disconnected"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    // run's early error path drops its producer and joins before returning.
    let error = live_dashboard::run(FailedOutput, 2).unwrap_err();
    assert!(error.contains("dashboard output"));
}
