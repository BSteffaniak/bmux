use bmux_presenter_runtime::{
    HostError,
    mirror::{CanvasRect, MirrorFrame, WireCell, WireImage, WireImagePayload},
};
fn frame() -> MirrorFrame {
    MirrorFrame {
        interaction_revision: None,
        private_scene: None,
        playbook: None,
        surface_id: "surface".into(),
        slide_id: "slide".into(),
        revision: 1,
        width: 2,
        height: 1,
        cells: vec![
            WireCell {
                symbol: " ".into(),
                width: 1,
                continuation: false,
                fg: None,
                bg: None,
                modifiers: 0
            };
            2
        ],
        images: vec![],
    }
}
#[test]
fn rejects_bad_graphemes_continuations_and_controls() {
    let mut value = frame();
    value.cells[0].symbol = "ab".into();
    assert_eq!(value.validate(100, 8192), Err(HostError::Invalid));
    let mut value = frame();
    value.cells[0].symbol = "界".into();
    value.cells[0].width = 2;
    assert_eq!(value.validate(100, 8192), Err(HostError::Invalid));
    value.cells[1].width = 0;
    value.cells[1].symbol.clear();
    value.cells[1].continuation = true;
    assert!(value.validate(100, 8192).is_ok());
    value.cells[0].symbol = "\u{1b}".into();
    assert_eq!(value.validate(100, 8192), Err(HostError::Invalid));
}
#[test]
fn images_reject_wrong_size_huge_dimensions_duplicate_keys_and_bad_bounds() {
    let image = WireImage {
        fit: bmux_tui::image::ImageFit::Stretch,
        allocation: None,
        key: "image".into(),
        destination: CanvasRect {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        },
        clip: CanvasRect {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        },
        payload: WireImagePayload::Pixels {
            bytes: vec![0; 3],
            width: 1,
            height: 1,
            rgba: false,
        },
    };
    let mut value = frame();
    value.images.push(image.clone());
    assert!(value.validate(100, 8192).is_ok());
    value.images[0].fit = bmux_tui::image::ImageFit::Contain;
    value.images[0].allocation = Some(bmux_tui::image::ImageBounds {
        x: 0,
        y: -1,
        width: 2,
        height: 2,
    });
    let roundtrip: MirrorFrame =
        serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(roundtrip.images[0].fit, bmux_tui::image::ImageFit::Contain);
    assert_eq!(roundtrip.images[0].allocation, value.images[0].allocation);
    assert!(roundtrip.validate(100, 8192).is_ok());
    value.images[0].allocation = Some(bmux_tui::image::ImageBounds {
        x: 1,
        y: 0,
        width: 1,
        height: 1,
    });
    assert_eq!(value.validate(100, 8192), Err(HostError::Invalid));
    value.images[0].allocation = None;
    value.images.push(image);
    assert_eq!(value.validate(100, 8192), Err(HostError::Invalid));
    value.images.pop();
    value.images[0].destination.x = 1;
    assert_eq!(value.validate(100, 8192), Err(HostError::Invalid));
    value.images[0].destination.x = 0;
    value.images[0].payload = WireImagePayload::Pixels {
        bytes: vec![],
        width: u32::MAX,
        height: 1,
        rgba: true,
    };
    assert_eq!(value.validate(100, 8192), Err(HostError::Limit));
    value.images[0].payload = WireImagePayload::Pixels {
        bytes: vec![],
        width: 1,
        height: 1,
        rgba: true,
    };
    assert_eq!(value.validate(100, 8192), Err(HostError::Invalid));
    value.images[0].payload = WireImagePayload::Png {
        bytes: vec![1, 2, 3],
        width: 1,
        height: 1,
    };
    assert_eq!(value.validate(100, 8192), Err(HostError::Invalid));
}
#[test]
fn placement_maps_only_committed_visible_canvas() {
    let value = frame();
    let placement = value.placement(6, 5);
    assert_eq!(placement.input_point(2, 2), Some((0, 0)));
    assert_eq!(placement.input_point(1, 2), None);
    assert_eq!(placement.input_point(4, 2), None);
}
#[test]
fn component_layout_letterboxes_without_changing_owner_canvas() {
    use bmux_presenter_runtime::MirrorComponent;
    use bmux_tui::{
        component::{Component, Constraints, LayoutCx},
        geometry::Size,
    };
    let value = frame();
    let component = MirrorComponent::new(&value, 100, 8192).unwrap();
    let layout = component.layout(
        Constraints::tight(Size::new(6, 5)),
        &mut LayoutCx::default(),
    );
    assert_eq!(component.input_point(&layout, 2, 2), Some((0, 0)));
    assert_eq!(component.input_point(&layout, 1, 2), None);
    assert_eq!((value.width, value.height), (2, 1));
}

#[test]
fn all_modifiers_round_trip_through_bmux_paint() {
    use bmux_tui::{
        buffer::Buffer, frame::Frame, geometry::Rect, image_scene::ImageScene, paint::PaintCx,
    };
    let mut value = frame();
    value.cells[0].modifiers = 255;
    let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
    let mut native = Frame::new(&mut buffer);
    value
        .paint(&mut PaintCx::new(&mut native), 100, 8192)
        .unwrap();
    let captured = MirrorFrame::capture(
        "surface".into(),
        "slide".into(),
        1,
        &native,
        &ImageScene::default(),
        100,
        8192,
    )
    .unwrap();
    assert_eq!(captured.cells[0].modifiers, 255);
}

#[test]
fn legacy_wire_adapter_preserves_json_and_native_unicode_styles() {
    use bmux_tui::{
        component::{Component, Constraints, LayoutCx},
        embedded_surface::EmbeddedSurface,
        geometry::Size,
    };
    let mut value = frame();
    value.cells[0].symbol = "界".into();
    value.cells[0].width = 2;
    value.cells[0].modifiers = 255;
    value.cells[1].symbol.clear();
    value.cells[1].width = 0;
    value.cells[1].continuation = true;
    value.cells[1].modifiers = 255;
    let wire = serde_json::to_value(&value).unwrap();
    let restored: MirrorFrame = serde_json::from_value(wire.clone()).unwrap();
    let retained = restored.retained_surface(17, 100, 8192).unwrap();
    assert_eq!(serde_json::to_value(&restored).unwrap(), wire);
    assert_eq!(retained.identity().owner, "surface");
    assert_eq!(retained.identity().generation, 17);
    assert_eq!(retained.buffer().cells()[0].symbol, "界");
    assert!(retained.buffer().cells()[1].is_wide_continuation());
    let component = EmbeddedSurface::new(&retained);
    let committed = component.layout(
        Constraints::tight(Size::new(6, 5)),
        &mut LayoutCx::default(),
    );
    let point = component.input_point(&committed, 2, 2).unwrap();
    assert_eq!((point.x, point.y), (0, 0));
    // The serialized legacy schema remains product-specific; the reusable
    // retained identity does not acquire the slide label.
    assert_eq!(wire["slide_id"], "slide");
    assert!(wire.get("generation").is_none());
}

#[test]
fn mirror_component_rejects_geometry_from_another_revision() {
    use bmux_presenter_runtime::MirrorComponent;
    use bmux_tui::{
        component::{Component, Constraints, LayoutCx},
        geometry::Size,
    };
    let first = frame();
    let mut next = frame();
    next.revision += 1;
    let original = MirrorComponent::new(&first, 100, 8192).unwrap();
    let committed = original.layout(
        Constraints::tight(Size::new(6, 5)),
        &mut LayoutCx::default(),
    );
    let replacement = MirrorComponent::new(&next, 100, 8192).unwrap();
    assert!(replacement.input_point(&committed, 2, 2).is_none());
}
