//! Native queue/reducer fences use the actual observer commit, not a shadow scene.
use super::tests::{attach_view_state_with_scrollback_fixture, disconnected_focus_test_client};
use super::*;
use bmux_frontend_runtime::input::{KeyPhase, Modifiers, PointerButton};
use bmux_frontend_runtime::presentation::PresentationIdentity;

fn native_attach_input<'a, T: AttachHost + ?Sized>(
    event: NativeAttachEvent,
    client: &'a mut StreamingBmuxClient,
    view: &'a mut AttachViewState,
    host: &T,
) -> impl Future<Output = Result<Option<Event>>> + Send + use<'a, T> {
    let pointer_is_current = native_pointer_event_is_current(&event, view, host);
    super::native_attach_input(event, client, view, pointer_is_current, true)
}

fn fixture() -> (
    AttachViewState,
    NativeAttachHandle,
    NativeAttachHost,
    AttachLayoutState,
) {
    let fixture = attach_view_state_with_scrollback_fixture();
    let layout = fixture.cached_layout_state.clone().unwrap();
    let mut view = AttachViewState::with_private_presentation(bmux_client::AttachOpenInfo {
        context_id: None,
        session_id: layout.session_id,
        can_write: true,
    });
    view.pane_buffers = fixture.pane_buffers;
    view.presentation_only = true;
    let (handle, mut host) = NativeAttachHost::channel(80, 24, 7, |_| Ok(())).unwrap();
    view.set_observer(host.observer.take().unwrap());
    render(&mut view, &layout);
    (view, handle, host, layout)
}

fn render(view: &mut AttachViewState, layout: &AttachLayoutState) {
    view.dirty.mark_full_frame(AttachDirtySource::ManualRedraw);
    render_attach_frame_to_output(
        None,
        view,
        layout,
        &RuntimeAppearance::default(),
        &[],
        0,
        &bmux_config::DamageBehaviorConfig::default(),
        u64::MAX,
        &mut DisplayCaptureFanout::default(),
        TerminalGeometry { cols: 80, rows: 24 },
        None,
    )
    .unwrap();
}

fn identity(view: &AttachViewState) -> PresentationIdentity {
    view.observer
        .as_ref()
        .unwrap()
        .committed_prepared_identity()
        .unwrap()
}

fn mouse(identity: PresentationIdentity) -> NativeAttachEvent {
    NativeAttachEvent::Mouse {
        identity,
        column: 1,
        row: 1,
        button: PointerButton::Left,
        phase: KeyPhase::Release,
        modifiers: Modifiers::default(),
    }
}

async fn pop(host: &mut NativeAttachHost) -> NativeAttachEvent {
    let Some(AttachHostEvent::Native(event)) = host.next_event().await.unwrap() else {
        panic!("expected native event");
    };
    event
}

#[tokio::test]
async fn queued_resize_blocks_projection_until_latest_geometry_is_applied() {
    let (_view, handle, mut host, _) = fixture();
    assert!(!host.viewport_update_pending());
    for (generation, columns) in [(8, 100), (9, 120)] {
        handle
            .try_send(NativeAttachEvent::Resize {
                columns,
                rows: 30,
                viewport_generation: generation,
            })
            .unwrap();
    }
    assert!(host.viewport_update_pending());
    let _superseded = pop(&mut host).await;
    assert!(host.viewport_update_pending());
    let NativeAttachEvent::Resize {
        columns,
        rows,
        viewport_generation,
    } = pop(&mut host).await
    else {
        panic!("expected latest resize");
    };
    host.native_resize(columns, rows, viewport_generation)
        .unwrap();
    assert!(!host.viewport_update_pending());
    assert_eq!(host.geometry().cols, 120);
    assert_eq!(host.viewport_generation(), Some(9));
}

#[tokio::test]
async fn queued_pointer_is_rejected_after_resize_admission_before_resize_dequeue() {
    let (mut view, handle, mut host, _) = fixture();
    let id = identity(&view);
    assert!(native_pointer_event_is_current(&mouse(id), &view, &host));
    handle.try_send(mouse(id)).unwrap();
    handle
        .try_send(NativeAttachEvent::Resize {
            columns: 90,
            rows: 30,
            viewport_generation: 8,
        })
        .unwrap();
    let mut client = disconnected_focus_test_client().await;
    let event = pop(&mut host).await;
    assert!(
        native_attach_input(event, &mut client, &mut view, &host)
            .await
            .unwrap()
            .is_none()
    );
    // The observer still describes the old displayed frame: FIFO alone would
    // have accepted it. The live viewport fence has already advanced.
    assert_eq!(identity(&view), id);
    assert_eq!(host.geometry().cols, 80);
    assert_eq!(host.viewport_generation(), Some(8));
}

#[tokio::test]
async fn expanded_scroll_and_internal_release_recheck_actual_commit() {
    let (mut view, handle, mut host, layout) = fixture();
    let id = identity(&view);
    handle
        .try_send(NativeAttachEvent::Scroll {
            identity: id,
            column: 1,
            row: 1,
            delta_x: 0,
            delta_y: 2,
            modifiers: Modifiers::default(),
        })
        .unwrap();
    let mut client = disconnected_focus_test_client().await;
    assert!(
        native_attach_input(pop(&mut host).await, &mut client, &mut view, &host)
            .await
            .unwrap()
            .is_some()
    );
    render(&mut view, &layout);
    assert_ne!(identity(&view).revision, id.revision);
    assert!(
        native_attach_input(pop(&mut host).await, &mut client, &mut view, &host)
            .await
            .unwrap()
            .is_none()
    );
    host.queue_native_event(mouse(id));
    assert!(
        native_attach_input(pop(&mut host).await, &mut client, &mut view, &host)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn suspend_invalidates_pending_scroll_and_handle_clones_share_fence() {
    let (view, handle, mut host, _) = fixture();
    let id = identity(&view);
    handle
        .try_send(NativeAttachEvent::Scroll {
            identity: id,
            column: 1,
            row: 1,
            delta_x: 2,
            delta_y: 0,
            modifiers: Modifiers::default(),
        })
        .unwrap();
    assert!(native_pointer_event_is_current(
        &pop(&mut host).await,
        &view,
        &host
    ));
    handle
        .clone()
        .try_send(NativeAttachEvent::Suspend {
            viewport_generation: 8,
        })
        .unwrap();
    assert!(!native_pointer_event_is_current(
        &pop(&mut host).await,
        &view,
        &host
    ));
}

#[tokio::test]
async fn non_passthrough_binding_mode_blocks_committed_text_without_pty_io() {
    let (mut view, _, _, _) = fixture();
    let mut config = BmuxConfig::default();
    config.keybindings.modes.insert(
        "native-command".into(),
        bmux_config::keybind::ModeBindingConfig {
            label: "Command".into(),
            passthrough: false,
            bindings: BTreeMap::new(),
        },
    );
    assert!(!native_text_passthrough(&config, Some("native-command")));
    assert!(native_text_passthrough(&config, None));
    let mut client = disconnected_focus_test_client().await;
    // An attempted pane write on this disconnected client would fail.
    native_committed_text(&mut client, &mut view, "committed", false)
        .await
        .unwrap();
}

#[tokio::test]
async fn native_committed_text_failure_is_recoverable_and_reported() {
    let (mut view, _, _, layout) = fixture();
    view.cached_layout_state = Some(layout);
    let mut client = disconnected_focus_test_client().await;
    native_committed_text(&mut client, &mut view, "i", true)
        .await
        .expect("input rejection must not terminate the native host");
    assert!(
        view.transient_status
            .as_ref()
            .is_some_and(|status| status.starts_with("input failed:"))
    );
}

#[test]
fn lifecycle_backpressure_fails_closed_for_already_queued_pointer() {
    let (view, handle, mut host, _) = fixture();
    let id = identity(&view);
    for _ in 0..256 {
        handle.try_send(mouse(id)).unwrap();
    }
    assert!(
        handle
            .try_send(NativeAttachEvent::Suspend {
                viewport_generation: 8
            })
            .is_err()
    );
    assert_eq!(host.viewport_generation(), Some(8));
    let queued = host.events.try_recv().unwrap();
    assert!(!native_pointer_event_is_current(
        &queued.event,
        &view,
        &host
    ));
}

#[tokio::test]
async fn stale_hover_and_reconnected_epoch_are_rejected_by_reducer() {
    let (mut view, handle, mut host, layout) = fixture();
    let old = identity(&view);
    handle
        .try_send(NativeAttachEvent::Motion {
            identity: old,
            column: 1,
            row: 1,
            drag: None,
            modifiers: Modifiers::default(),
        })
        .unwrap();
    // Rebinding creates a new observer epoch, even with equal target, generation,
    // and first revision. The old queued cell position is not authoritative.
    let (_, mut replacement) = NativeAttachHost::channel(80, 24, 7, |_| Ok(())).unwrap();
    view.set_observer(replacement.observer.take().unwrap());
    render(&mut view, &layout);
    assert_ne!(identity(&view).epoch, old.epoch);
    let mut client = disconnected_focus_test_client().await;
    assert!(
        native_attach_input(pop(&mut host).await, &mut client, &mut view, &replacement)
            .await
            .unwrap()
            .is_none()
    );
}

#[test]
fn epoch_target_generation_revision_and_receipt_must_all_match() {
    let (mut view, _, host, _) = fixture();
    let id = identity(&view);
    for stale in [
        PresentationIdentity {
            epoch: Uuid::new_v4(),
            ..id
        },
        PresentationIdentity {
            attached_id: Uuid::new_v4(),
            ..id
        },
        PresentationIdentity {
            viewport_generation: 8,
            ..id
        },
        PresentationIdentity {
            revision: id.revision + 1,
            ..id
        },
        PresentationIdentity { revision: 0, ..id },
    ] {
        assert!(!native_pointer_event_is_current(
            &mouse(stale),
            &view,
            &host
        ));
    }
    host.accepted_revision
        .store(0, std::sync::atomic::Ordering::Release);
    assert!(!native_pointer_event_is_current(&mouse(id), &view, &host));
    host.accepted_revision
        .store(id.revision, std::sync::atomic::Ordering::Release);
    // Changing the live target invalidates old input even before new output.
    view.attached_id = Uuid::new_v4();
    assert!(!native_pointer_event_is_current(&mouse(id), &view, &host));
}

#[tokio::test]
async fn native_text_plugin_capture_has_one_owner_and_never_falls_through_to_pty() {
    use bmux_plugin::surface::{
        PluginSurface, PluginSurfaceId, PluginSurfaceRegion, PluginSurfaceSnapshot,
        PluginSurfaceTarget,
    };
    let (mut view, _, _, layout) = fixture();
    let endpoint = bmux_plugin::AttachInputEndpoint {
        capability: "native-text-test".into(),
        interface_id: "input".into(),
        operation: "edit".into(),
    };
    let mut surface = PluginSurface::layout(
        PluginSurfaceId::new("native-text-test", "edit", Uuid::from_u128(702)),
        1,
        bmux_plugin::layout::PluginLayoutId::new("native-text-test", "layout"),
        vec![RenderOp::text_run(
            0,
            0,
            "edit",
            bmux_plugin::RenderStyle::new(),
        )],
    )
    .interactive_region(
        PluginSurfaceRegion::new("field", ExtensionRect::new(0, 0, 4, 1))
            .endpoint(endpoint.clone())
            .focusable(bmux_plugin::surface::PluginSurfaceCursor::Text),
    );
    surface.modal = true;
    surface.layer = 100;
    surface.target = PluginSurfaceTarget::Explicit(ExtensionRect::new(0, 0, 4, 1));
    view.presentation_surfaces
        .publish(
            "native-text-test",
            PluginSurfaceSnapshot {
                revision: 1,
                surfaces: vec![surface],
            },
        )
        .unwrap();
    view.dirty
        .mark_retained_surfaces_dirty(AttachDirtySource::SceneChanged);
    render(&mut view, &layout);
    let hit = view.retained_compositor.hit_test(0, 0).unwrap();
    view.plugin_focus.focus_hit(&view.retained_compositor, &hit);
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    view.presentation_input.register_paste(
        endpoint.clone(),
        Arc::new(move |hook, text| {
            sink.lock()
                .unwrap()
                .push((hook.to_owned(), text.to_owned()));
            bmux_plugin::AttachInputResult {
                consumed: true,
                ..Default::default()
            }
        }),
    );
    let mut client = disconnected_focus_test_client().await;
    native_committed_text(&mut client, &mut view, "日本語", true)
        .await
        .unwrap();
    assert_eq!(
        *received.lock().unwrap(),
        vec![(
            "native-text-test:edit:field".to_owned(),
            "日本語".to_owned()
        )]
    );
    // An unconsumed edit in a modal scope cannot fall through to a pane.
    view.presentation_input.register_paste(
        endpoint,
        Arc::new(|_, _| bmux_plugin::AttachInputResult::default()),
    );
    native_committed_text(&mut client, &mut view, "modal-blocked", true)
        .await
        .unwrap();
    view.help_overlay_open = true;
    assert!(native_capture_committed_text(&mut view, "blocked"));
    assert_eq!(received.lock().unwrap().len(), 1);
}
