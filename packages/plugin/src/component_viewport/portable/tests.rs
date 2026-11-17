use super::*;
use bmux_tui::{
    buffer::Buffer,
    damage::Damage,
    frame::Frame,
    geometry::{Point, Rect},
    hit::HitRegion,
    image::{ImageFit, ImagePayload, ImagePixelFormat, ImagePlacement},
    presentation::PresentationState,
    selection::SelectionScene,
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

fn resources(owner: &str) -> FrontendPresentationResources {
    FrontendPresentationResources::new(owner.into(), 7, limits()).unwrap()
}

#[test]
fn scene_epoch_separates_namespaces_without_issuing_runtime_authority() {
    let mut first = FrontendPresentationResources::new("same-owner".into(), 1, limits()).unwrap();
    let mut second = FrontendPresentationResources::new("same-owner".into(), 2, limits()).unwrap();
    let first_id = first.create(1).unwrap();
    let second_id = second.create(1).unwrap();
    first.publish(first_id, 1, 1, snapshot()).unwrap();
    second.publish(second_id, 1, 1, snapshot()).unwrap();
    let first_scene = first.prepare(first_id, 1).unwrap();
    let second_scene = second.prepare(second_id, 1).unwrap();
    assert_ne!(first_scene.scoped("field"), second_scene.scoped("field"));
    // Namespace epochs do not mark any output displayed or admit input.
    assert!(first.committed_for_input(first_id, 1, 1).is_err());
    assert!(second.committed_for_input(second_id, 1, 1).is_err());
}

#[test]
fn publication_and_failed_display_never_enable_uncommitted_input() {
    let mut resources = resources("owner");
    let id = resources.create(2).unwrap();
    resources.publish(id, 2, 10, snapshot()).unwrap();
    assert_eq!(
        resources.validate_input(id, 2, 10),
        Err(PortablePresentationError::NotCommitted)
    );
    resources.acknowledge(id, 2, 10).unwrap();
    resources.publish(id, 2, 11, snapshot()).unwrap();
    resources.validate_input(id, 2, 10).unwrap();
    assert_eq!(
        resources.validate_input(id, 2, 11),
        Err(PortablePresentationError::Stale)
    );
    resources.display_failed();
    assert_eq!(
        resources.validate_input(id, 2, 10),
        Err(PortablePresentationError::NotCommitted)
    );
    resources.acknowledge(id, 2, 11).unwrap();
    resources.validate_input(id, 2, 11).unwrap();
    assert_eq!(
        resources.acknowledge(id, 2, 10),
        Err(PortablePresentationError::Stale)
    );
    assert_eq!(
        resources.publish(id, 2, 11, snapshot()),
        Err(PortablePresentationError::Stale)
    );
    assert_eq!(
        resources.publish(id, 3, 12, snapshot()),
        Err(PortablePresentationError::Stale)
    );
}

#[test]
fn replay_uses_authoritative_transform_for_all_metadata() {
    let mut resources = resources("owner");
    let id = resources.create(2).unwrap();
    resources.publish(id, 2, 1, snapshot()).unwrap();
    let replay = resources.prepare(id, 1).unwrap();
    let state = PresentationState::new(Rect::new(0, 0, 12, 6));
    let mut prepared = state
        .prepare(Damage::Full, |cx| {
            cx.with_child(3, 2, LocalRect::new(0, 0, 4, 2), |cx| replay.paint(cx));
        })
        .unwrap();
    prepared
        .set_selection_text(replay.selection_text())
        .unwrap();
    assert_eq!(prepared.buffer().get(Point::new(3, 2)).unwrap().symbol, "a");
    assert_eq!(prepared.hits().regions()[0].area, Rect::new(3, 2, 2, 1));
    assert_eq!(
        prepared.semantics().regions()[0].area,
        Rect::new(3, 2, 2, 1)
    );
    assert_eq!(
        prepared.selection().fragments()[0].area,
        Rect::new(3, 2, 1, 1)
    );
    assert_eq!(prepared.cursor().unwrap().position, Point::new(4, 2));
    assert_ne!(prepared.focus_scope().unwrap().as_str(), "modal");
    let content = &prepared.selection().fragments()[0].content_id;
    assert_eq!(
        prepared.selection_text().get(content).map(String::as_str),
        Some("abc")
    );
    assert_eq!(replay.text(content), Some("abc"));
}

#[test]
fn separate_owner_activations_never_share_focus_or_source_identity() {
    let mut first = resources("first");
    let mut second = resources("second");
    let a = first.create(1).unwrap();
    let b = second.create(1).unwrap();
    first.publish(a, 1, 1, snapshot()).unwrap();
    second.publish(b, 1, 1, snapshot()).unwrap();
    let a = first.prepare(a, 1).unwrap();
    let b = second.prepare(b, 1).unwrap();
    let state = PresentationState::new(Rect::new(0, 0, 12, 6));
    let pa = state.prepare(Damage::Full, |cx| a.paint(cx)).unwrap();
    let pb = state.prepare(Damage::Full, |cx| b.paint(cx)).unwrap();
    assert_ne!(pa.hits().regions()[0].id, pb.hits().regions()[0].id);
    assert_ne!(
        pa.selection().fragments()[0].content_id,
        pb.selection().fragments()[0].content_id
    );
    assert_eq!(a.text(&pb.selection().fragments()[0].content_id), None);
}

#[test]
fn destroyed_resource_revokes_retained_replays_and_handles_are_not_reused() {
    let mut resources = resources("owner");
    let id = resources.create(1).unwrap();
    resources.publish(id, 1, 1, snapshot()).unwrap();
    let replay = resources.prepare(id, 1).unwrap();
    resources.destroy(id).unwrap();
    assert_ne!(resources.create(1).unwrap(), id);
    let state = PresentationState::new(Rect::new(0, 0, 4, 2));
    let prepared = state.prepare(Damage::Full, |cx| replay.paint(cx)).unwrap();
    assert_eq!(prepared.hits().regions(), []);
    assert_eq!(prepared.selection().fragments(), []);
    assert_eq!(
        resources.validate_input(id, 1, 1),
        Err(PortablePresentationError::UnknownResource)
    );
}

#[test]
fn backpressure_does_not_evict_unacknowledged_output_or_guess_revision() {
    let mut resources = FrontendPresentationResources::new(
        "owner".into(),
        1,
        PortablePresentationLimits {
            max_pending: 1,
            ..limits()
        },
    )
    .unwrap();
    let id = resources.create(1).unwrap();
    resources.publish(id, 1, 1, snapshot()).unwrap();
    assert_eq!(
        resources.publish(id, 1, 2, snapshot()),
        Err(PortablePresentationError::Limit)
    );
    assert_eq!(
        resources.acknowledge(id, 1, 2),
        Err(PortablePresentationError::Stale)
    );
    resources.acknowledge(id, 1, 1).unwrap();
    resources.publish(id, 1, 2, snapshot()).unwrap();
}

#[test]
fn selected_port_separates_producer_mutations_from_output_authority() {
    let host = RetainedFrontendHost::new("selected".into(), 9, limits()).unwrap();
    let port = host.producer();
    let FrontendPresentationResponse::Created { resource } = port
        .apply(FrontendPresentationRequest::Create { generation: 2 })
        .unwrap()
    else {
        panic!("expected created resource");
    };
    port.apply(FrontendPresentationRequest::Publish {
        resource,
        generation: 2,
        revision: 1,
        presentation: Box::new(snapshot()),
    })
    .unwrap();
    let replay = host.prepare(resource, 1).unwrap();
    assert_eq!(
        host.validate_input(resource, 2, 1),
        Err(PortablePresentationError::NotCommitted)
    );
    host.acknowledge(resource, 2, 1).unwrap();
    host.validate_input(resource, 2, 1).unwrap();
    host.display_failed().unwrap();
    assert_eq!(
        host.validate_input(resource, 2, 1),
        Err(PortablePresentationError::NotCommitted)
    );
    assert_eq!(
        port.extension("unsupported", b"{}"),
        Err(PortablePresentationError::Version)
    );
    drop(host);
    assert_eq!(
        port.apply(FrontendPresentationRequest::Create { generation: 2 }),
        Err(PortablePresentationError::Unavailable)
    );
    let state = PresentationState::new(Rect::new(0, 0, 4, 2));
    let prepared = state.prepare(Damage::Full, |cx| replay.paint(cx)).unwrap();
    assert_eq!(prepared.hits().regions(), []);
}

#[test]
fn committed_input_snapshot_is_revoked_without_reviving_after_display_repair() {
    let host = RetainedFrontendHost::new("input".into(), 1, limits()).unwrap();
    let producer = host.producer();
    let FrontendPresentationResponse::Created { resource } = producer
        .apply(FrontendPresentationRequest::Create { generation: 1 })
        .unwrap()
    else {
        panic!("expected resource");
    };
    producer
        .apply(FrontendPresentationRequest::Publish {
            resource,
            generation: 1,
            revision: 1,
            presentation: Box::new(snapshot()),
        })
        .unwrap();
    assert!(host.committed_for_input(resource, 1, 1).is_err());
    host.acknowledge(resource, 1, 1).unwrap();
    let input = host.committed_for_input(resource, 1, 1).unwrap();
    let text = input.selection_text();
    let (content, _) = text.first_key_value().unwrap();
    assert_eq!(input.text(content), Some("abc"));
    host.display_failed().unwrap();
    assert_eq!(input.text(content), None);
    host.acknowledge(resource, 1, 1).unwrap();
    assert_eq!(input.text(content), None);
    let fresh = host.committed_for_input(resource, 1, 1).unwrap();
    assert_eq!(fresh.text(content), Some("abc"));
    producer
        .apply(FrontendPresentationRequest::Publish {
            resource,
            generation: 1,
            revision: 2,
            presentation: Box::new(snapshot()),
        })
        .unwrap();
    // Pending output cannot invalidate the still-displayed source.
    assert_eq!(fresh.text(content), Some("abc"));
    host.acknowledge(resource, 1, 2).unwrap();
    assert_eq!(fresh.text(content), None);
    assert!(host.committed_for_input(resource, 1, 1).is_err());
}

fn image() -> ImagePlacement {
    ImagePlacement {
        key: ImageKey::new("image"),
        payload: ImagePayload::Pixels {
            bytes: vec![255; 16].into(),
            width: 2,
            height: 2,
            format: ImagePixelFormat::Rgba8,
        },
        destination: Rect::new(0, 0, 2, 1),
        clip: Rect::new(0, 0, 2, 1),
        allocation: None,
        fit: ImageFit::Contain,
        lifecycle: ImageLifecycle::Persistent,
    }
}

#[test]
fn reconciled_capture_keeps_persistent_images_without_replaying_old_operations() {
    let mut state = PresentationState::new(Rect::new(0, 0, 4, 2));
    state
        .present(
            Damage::Full,
            |cx| cx.push_image(ImageContribution::Present(image())),
            |_, _| Ok::<(), ()>(()),
        )
        .unwrap();
    let prepared = state.prepare(Damage::Full, |_| {}).unwrap();
    assert_eq!(prepared.images(), []);
    assert_eq!(prepared.image_scene().placements().len(), 1);
    let value = PortablePresentation::from_prepared(&prepared);
    assert_eq!(value.images.len(), 1);
    assert_eq!(value.occlusions, []);
    value.validate(limits()).unwrap();
    let mut store = resources("images");
    let id = store.create(1).unwrap();
    store.publish(id, 1, 1, value).unwrap();
    let replay = store.prepare(id, 1).unwrap();
    let destination = PresentationState::new(Rect::new(0, 0, 8, 4));
    let staged = destination
        .prepare(Damage::Full, |cx| {
            cx.with_child(3, 2, LocalRect::new(0, 0, 4, 2), |cx| replay.paint(cx));
        })
        .unwrap();
    let placed = staged.image_scene().placements().next().unwrap();
    assert_eq!(placed.destination, Rect::new(3, 2, 2, 1));
    assert_eq!(placed.lifecycle, ImageLifecycle::Frame);
    assert_ne!(placed.key, ImageKey::new("image"));
}

#[test]
fn reconciled_capture_occludes_host_images_without_occluding_final_producer_output() {
    let producer = PresentationState::new(Rect::new(0, 0, 4, 2));
    let prepared = producer
        .prepare(Damage::Full, |cx| {
            cx.occlude(LocalRect::new(0, 0, 2, 1));
            cx.push_image(ImageContribution::Present(image()));
            cx.push_hit(HitRegion::new("top", Rect::new(0, 0, 2, 1)));
        })
        .unwrap();
    let value = PortablePresentation::from_prepared(&prepared);
    assert_eq!(value.occlusions, vec![(0, Rect::new(0, 0, 2, 1))]);
    let mut store = resources("occlusion");
    let id = store.create(1).unwrap();
    store.publish(id, 1, 1, value).unwrap();
    let replay = store.prepare(id, 1).unwrap();
    let host = PresentationState::new(Rect::new(0, 0, 4, 2));
    let staged = host
        .prepare(Damage::Full, |cx| {
            let mut background = image();
            background.key = ImageKey::new("background");
            cx.push_image(ImageContribution::Present(background));
            replay.paint(cx);
        })
        .unwrap();
    assert_eq!(staged.image_scene().placements().len(), 1);
    assert_eq!(staged.hits().regions().len(), 1);
    assert_ne!(
        staged.image_scene().placements().next().unwrap().key,
        ImageKey::new("background")
    );
}

#[test]
fn modal_declarations_use_existing_commit_and_restore_authority() {
    let mut value = snapshot();
    value.modal_scopes = vec![HitId::new("modal")];
    value.validate(limits()).unwrap();
    let mut store = resources("modal-owner");
    let id = store.create(1).unwrap();
    store.publish(id, 1, 1, value).unwrap();
    let replay = store.prepare(id, 1).unwrap();
    let mut state = PresentationState::new(Rect::new(0, 0, 4, 2));
    let mut staged = state.prepare(Damage::Full, |cx| replay.paint(cx)).unwrap();
    assert!(staged.set_modal_scopes(&replay.modal_scopes()));
    assert_eq!(state.modal_scopes().len(), 0);
    state
        .present_prepared(Some(staged), |_, _| Ok::<(), ()>(()))
        .unwrap();
    assert_eq!(state.modal_scopes().len(), 1);
    assert_eq!(
        state.focus().active().map(HitId::as_str),
        Some(replay.scoped("field").as_str())
    );
    let closing_value = {
        let mut value = snapshot();
        value.focus_scope = None;
        value
    };
    store.publish(id, 1, 2, closing_value).unwrap();
    let closing = store.prepare(id, 2).unwrap();
    let mut staged = state.prepare(Damage::Full, |cx| closing.paint(cx)).unwrap();
    assert!(staged.set_modal_scopes(&closing.modal_scopes()));
    assert_eq!(state.modal_scopes().len(), 1);
    state
        .present_prepared(Some(staged), |_, _| Ok::<(), ()>(()))
        .unwrap();
    assert_eq!(state.modal_scopes().len(), 0);
}

#[test]
fn v1_extension_is_not_silently_treated_as_modal_capable_v2() {
    let host = RetainedFrontendHost::new("versioned".into(), 1, limits()).unwrap();
    assert_eq!(
        host.producer()
            .extension("bmux.presentation.retained.v1", b"{}"),
        Err(PortablePresentationError::Version)
    );
    let mut value = snapshot();
    value.version = 1;
    assert_eq!(
        value.validate(limits()),
        Err(PortablePresentationError::Version)
    );
}

#[test]
fn clipped_selection_never_maps_partial_source_units() {
    let mut value = snapshot();
    value.selection = Some({
        let mut selection = SelectionScene::new();
        selection.push_scope(SelectionScope::new("scope", Rect::new(0, 0, 4, 2)));
        selection.push_fragment(SelectionFragment::new(
            "scope",
            "content",
            Rect::new(0, 0, 2, 1),
            0,
            0..2,
        ));
        selection
    });
    let mut store = resources("clipped");
    let id = store.create(1).unwrap();
    store.publish(id, 1, 1, value).unwrap();
    let replay = store.prepare(id, 1).unwrap();
    let state = PresentationState::new(Rect::new(0, 0, 3, 2));
    let frame = state
        .prepare(Damage::Full, |cx| {
            cx.with_child(-1, 0, LocalRect::new(0, 0, 4, 2), |cx| replay.paint(cx));
        })
        .unwrap();
    assert_eq!(frame.selection().fragments(), []);
    assert_eq!(frame.hits().regions()[0].area, Rect::new(0, 0, 1, 1));
    assert_eq!(frame.cursor().unwrap().position, Point::new(0, 0));
}

#[test]
fn failed_backend_does_not_install_portable_interaction() {
    let mut resources = resources("owner");
    let id = resources.create(1).unwrap();
    resources.publish(id, 1, 1, snapshot()).unwrap();
    let replay = resources.prepare(id, 1).unwrap();
    let mut state = PresentationState::new(Rect::new(0, 0, 4, 2));
    let result = state.present(
        Damage::Full,
        |cx| replay.paint(cx),
        |_, _| Err::<(), _>("output failed"),
    );
    assert_eq!(result, Err("output failed"));
    assert_eq!(state.hits().regions(), []);
    assert_eq!(
        resources.validate_input(id, 1, 1),
        Err(PortablePresentationError::NotCommitted)
    );
}
