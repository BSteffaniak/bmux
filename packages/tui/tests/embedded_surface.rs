use std::sync::Arc;

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
