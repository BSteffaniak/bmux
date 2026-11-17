use super::*;
use crate::{
    damage::Damage,
    geometry::Point,
    hit::HitId,
    image::{ImageFit, ImageKey, ImagePixelFormat, ImagePlacement},
    paint::PaintCx,
    presentation::PresentationState,
    selection::{SelectionFragment, SelectionScope},
    semantic::SemanticRegion,
    style::Style,
};

fn limits() -> PortablePresentationLimits {
    PortablePresentationLimits::default()
}

fn snapshot() -> PortablePresentation {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
    let mut frame = Frame::new(&mut buffer);
    let mut cx = PaintCx::new(&mut frame);
    cx.set_cell(0, 0, "a", Style::new());
    cx.set_focus_scope(Some(HitId::new("modal")));
    cx.push_hit(HitRegion::new("field", Rect::new(0, 0, 2, 1)).focusable(true));
    cx.push_semantic(SemanticRegion::new("label", Rect::new(0, 0, 2, 1), "text"));
    cx.push_selection_scope(SelectionScope::new("source", Rect::new(0, 0, 4, 2)).revision(3));
    cx.push_selection_fragment(
        SelectionFragment::new("source", "content", Rect::new(0, 0, 1, 1), 0, 0..1)
            .id("fragment")
            .revision(3),
    );
    cx.set_cursor(Point::new(1, 0), true);
    let mut snapshot = PortablePresentation::from_frame(&frame);
    snapshot
        .text
        .insert(SelectionContentId::new("content"), "abc".into());
    snapshot
}

#[test]
fn typed_and_wire_paths_share_admission_and_full_metadata() {
    let snapshot = snapshot();
    snapshot.validate(limits()).unwrap();
    let encoded = snapshot.encode(limits()).unwrap();
    assert_eq!(
        PortablePresentation::decode(&encoded, limits()).unwrap(),
        snapshot
    );
    let mut unknown = snapshot;
    unknown.version += 1;
    assert_eq!(
        unknown.validate(limits()),
        Err(PortablePresentationError::Version)
    );
    assert_eq!(
        PortablePresentation::decode(
            &encoded,
            PortablePresentationLimits {
                max_bytes: 1,
                ..limits()
            }
        ),
        Err(PortablePresentationError::Limit)
    );
}

#[test]
fn corrupt_source_boundaries_geometry_and_image_payloads_are_rejected() {
    let mut value = snapshot();
    value
        .text
        .insert(SelectionContentId::new("content"), "é".into());
    assert_eq!(
        value.validate(limits()),
        Err(PortablePresentationError::Invalid)
    );
    let mut value = snapshot();
    value.hits[0].area.x = 20;
    assert_eq!(
        value.validate(limits()),
        Err(PortablePresentationError::Invalid)
    );
    let mut value = snapshot();
    value
        .images
        .push(ImageContribution::Present(ImagePlacement {
            key: ImageKey::new("image"),
            payload: ImagePayload::Pixels {
                bytes: vec![0].into(),
                width: 2,
                height: 2,
                format: ImagePixelFormat::Rgba8,
            },
            destination: Rect::new(0, 0, 2, 1),
            clip: Rect::new(0, 0, 2, 1),
            allocation: None,
            fit: ImageFit::Contain,
            lifecycle: ImageLifecycle::Persistent,
            z: 0,
        }));
    assert_eq!(
        value.validate(limits()),
        Err(PortablePresentationError::Invalid)
    );
}

#[test]
fn reconciled_capture_is_neutral_and_preserves_complete_v2_scene() {
    let state = PresentationState::new(Rect::new(0, 0, 4, 2));
    let mut prepared = state
        .prepare(Damage::Full, |cx| {
            cx.occlude(crate::paint::LocalRect::new(0, 0, 4, 2));
            cx.set_focus_scope(Some(HitId::new("modal")));
            cx.push_hit(HitRegion::new("field", Rect::new(0, 0, 2, 1)).focusable(true));
            cx.push_selection_scope(SelectionScope::new("scope", Rect::new(0, 0, 4, 2)));
            cx.push_selection_fragment(SelectionFragment::new(
                "scope",
                "text",
                Rect::new(0, 0, 1, 1),
                0,
                0..1,
            ));
            cx.push_semantic(SemanticRegion::new(
                "semantic",
                Rect::new(0, 0, 1, 1),
                "text",
            ));
            cx.set_cursor(Point::new(1, 0), true);
        })
        .unwrap();
    assert!(prepared.set_modal_scopes(&[HitId::new("modal")]));
    prepared
        .set_selection_text([(SelectionContentId::new("text"), "x".into())].into())
        .unwrap();
    let value = PortablePresentation::from_prepared(&prepared);
    assert_eq!(value.version, 2);
    assert_eq!(value.modal_scopes, vec![HitId::new("modal")]);
    assert_eq!(value.occlusions, vec![(0, Rect::new(0, 0, 4, 2))]);
    assert_eq!(value.selection, Some(prepared.selection().clone()));
    assert_eq!(value.semantics, Some(prepared.semantics().clone()));
    assert_eq!(value.text, *prepared.selection_text());
    assert_eq!(
        PortablePresentation::decode(&value.encode(limits()).unwrap(), limits()).unwrap(),
        value
    );
}

#[test]
fn v2_rejects_v1_and_duplicate_modal_declarations() {
    let mut value = snapshot();
    value.version = 1;
    assert_eq!(
        value.validate(limits()),
        Err(PortablePresentationError::Version)
    );
    value.version = PORTABLE_PRESENTATION_VERSION;
    value.modal_scopes = vec![HitId::new("modal"), HitId::new("modal")];
    assert_eq!(
        value.validate(limits()),
        Err(PortablePresentationError::Invalid)
    );
}
