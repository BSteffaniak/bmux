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
    // Test-only authoritative capture-source token fixture; production obtains
    // this through capture_source before/after structured snapshot hydration.
    for pane in view.pane_buffers.keys() {
        view.pane_zoom
            .execution_tokens
            .insert(*pane, Uuid::new_v4());
    }
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

#[tokio::test]
async fn pane_zoom_requires_verified_execution_and_checks_geometry_before_intent() {
    let (mut view, handle, _host, layout) = fixture();
    let bound = handle.pane_zoom_target(None).unwrap();
    let mut client = disconnected_focus_test_client().await;
    // Image/cache incarnation cannot establish terminal execution continuity.
    view.image_execution_incarnations
        .insert(bound.pane_id, bound.execution_id);
    view.pane_zoom.execution_tokens.remove(&bound.pane_id);
    assert!(
        !super::super::pane_zoom_native::set_target(&mut view, &mut client, bound, 1500)
            .await
            .unwrap()
    );
    render(&mut view, &layout);
    assert!(handle.pane_zoom_target(None).is_none());
    view.pane_zoom
        .execution_tokens
        .insert(bound.pane_id, Uuid::new_v4());
    render(&mut view, &layout);
    assert_eq!(
        handle.refresh_pane_zoom_status(&bound),
        NativePaneZoomRefresh::Gone
    );
    let current = handle.pane_zoom_target(None).unwrap();
    let surface = view.retained_compositor.surfaces()[&current.view_id].clone();
    let mut surfaces = view
        .retained_compositor
        .surfaces()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let oversized = surfaces
        .iter_mut()
        .find(|surface| surface.id == current.view_id)
        .unwrap();
    oversized.content_rect = Some(bmux_attach_pipeline::DamageRect::new(0, 0, 2000, 1000));
    view.retained_compositor.replace_surfaces(
        surfaces,
        bmux_attach_pipeline::DamageRect::new(0, 0, 2000, 1000),
        bmux_attach_pipeline::DamageCoalescingPolicy::default(),
    );
    assert!(
        super::super::pane_zoom_native::set_target(&mut view, &mut client, current, 125)
            .await
            .is_err()
    );
    assert!(view.pane_zoom.requested.is_empty());
    assert_eq!(surface.id, current.view_id);
}

#[tokio::test]
async fn pane_zoom_status_distinguishes_contention_and_rejected_dequeue() {
    let (mut view, handle, _host, _layout) = fixture();
    let bound = handle.pane_zoom_target(None).unwrap();
    let before = match handle.refresh_pane_zoom_status(&bound) {
        NativePaneZoomRefresh::Ready {
            progress_revision, ..
        } => progress_revision,
        other => panic!("unexpected status: {other:?}"),
    };
    let guard = handle.3.lock().unwrap();
    assert_eq!(
        handle.refresh_pane_zoom_status(&bound),
        NativePaneZoomRefresh::Pending
    );
    drop(guard);
    handle.set_pane_zoom_max_scale(1250).unwrap();
    let refreshed = handle.refresh_pane_zoom_target(&bound).unwrap();
    assert_eq!(refreshed.max_scale_millis, 1250);
    let mut client = disconnected_focus_test_client().await;
    assert!(
        super::super::pane_zoom_native::set_target(&mut view, &mut client, refreshed, 1500)
            .await
            .is_err()
    );
    assert!(view.pane_zoom.requested.is_empty());
    super::super::pane_zoom_native::settle_dequeue(&view);
    match handle.refresh_pane_zoom_status(&bound) {
        NativePaneZoomRefresh::Ready {
            target,
            progress_revision,
        } => {
            assert_eq!(target.identity, bound.identity);
            assert_eq!(target.scale_millis, bound.scale_millis);
            assert!(progress_revision > before);
        }
        other => panic!("unexpected status: {other:?}"),
    }
    let mut gone = bound;
    gone.execution_id = Uuid::new_v4();
    assert_eq!(
        handle.refresh_pane_zoom_status(&gone),
        NativePaneZoomRefresh::Gone
    );
}

#[tokio::test]
async fn pane_zoom_refresh_stays_bound_and_only_observes_commits() {
    let (mut view, handle, _host, layout) = fixture();
    let bound = handle.pane_zoom_target(None).unwrap();
    let mut client = disconnected_focus_test_client().await;
    super::super::pane_zoom_native::set_target(&mut view, &mut client, bound, 1500)
        .await
        .unwrap();
    assert_eq!(handle.refresh_pane_zoom_target(&bound), Some(bound));
    render(&mut view, &layout);
    let latest = handle.refresh_pane_zoom_target(&bound).unwrap();
    assert_eq!(latest.scale_millis, 1500);
    assert_ne!(latest.identity.revision, bound.identity.revision);
    // Refresh never resolves another focused/hovered view or execution.
    let mut changed = bound;
    changed.view_id = Uuid::new_v4();
    assert!(handle.refresh_pane_zoom_target(&changed).is_none());
    changed = bound;
    changed.execution_id = Uuid::new_v4();
    assert!(handle.refresh_pane_zoom_target(&changed).is_none());
    changed = bound;
    changed.identity.epoch = Uuid::new_v4();
    assert!(handle.refresh_pane_zoom_target(&changed).is_none());
    let guard = handle.3.lock().unwrap();
    assert!(handle.refresh_pane_zoom_target(&bound).is_none());
    drop(guard);
    handle.1.store(
        bound.identity.viewport_generation + 1,
        std::sync::atomic::Ordering::Release,
    );
    assert!(handle.refresh_pane_zoom_target(&bound).is_none());
}

#[tokio::test]
async fn pane_zoom_two_native_views_keep_independent_scale_and_alternate_mode() {
    let (mut view, handle, _host, mut layout) = fixture();
    let mut second = layout.scene.surfaces[0].clone();
    second.id = Uuid::new_v4();
    second.rect.x += 20;
    second.content_rect.x += 20;
    second.cursor_owner = false;
    layout.scene.surfaces.push(second.clone());
    render(&mut view, &layout);
    let first = handle.pane_zoom_target(None).unwrap();
    let other = handle
        .pane_zoom_target(Some([
            f64::from(second.content_rect.x) + 0.25,
            f64::from(second.content_rect.y) + 0.25,
        ]))
        .unwrap();
    // Two retained views share one authoritative execution, not separate image
    // incarnations. Only their local presentation identity/state is distinct.
    assert_ne!(first.view_id, other.view_id);
    assert_eq!(first.pane_id, other.pane_id);
    assert_eq!(first.execution_id, other.execution_id);
    assert_eq!(
        first.execution_id,
        view.pane_zoom.execution_tokens[&first.pane_id]
    );
    let mut client = disconnected_focus_test_client().await;
    super::super::pane_zoom_native::set_target(&mut view, &mut client, first, 1500)
        .await
        .unwrap();
    render(&mut view, &layout);
    assert_eq!(
        handle
            .refresh_pane_zoom_target(&first)
            .unwrap()
            .scale_millis,
        1500
    );
    assert_eq!(
        handle
            .refresh_pane_zoom_target(&other)
            .unwrap()
            .scale_millis,
        1000
    );
    let grid = &mut view
        .pane_buffers
        .get_mut(&first.pane_id)
        .unwrap()
        .terminal_grid;
    let before = (grid.grid().width(), grid.grid().height());
    grid.process(b"\x1b[?1049hpositioned");
    render(&mut view, &layout);
    let grid = view.pane_buffers[&first.pane_id].terminal_grid.grid();
    assert_eq!(grid.mode(), bmux_terminal_grid::GridMode::Alternate);
    assert_eq!((grid.width(), grid.height()), before);
    assert!(view.pane_zoom.committed.contains_key(&first.view_id));
    view.pane_buffers
        .get_mut(&first.pane_id)
        .unwrap()
        .terminal_grid
        .process(b"\x1b[?1049l");
    render(&mut view, &layout);
    assert_eq!(
        view.pane_buffers[&first.pane_id]
            .terminal_grid
            .grid()
            .mode(),
        bmux_terminal_grid::GridMode::Main
    );
}

#[tokio::test]
#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
async fn pane_zoom_native_images_share_local_grid_and_do_not_duplicate_root() {
    use bmux_attach_image_protocol::{AttachImageProtocol, AttachPaneImage, CompressionId};
    let (mut view, handle, _host, layout) = fixture();
    let initial = handle.pane_zoom_target(None).unwrap();
    view.pane_buffers
        .get_mut(&initial.pane_id)
        .unwrap()
        .terminal_grid
        .process(b"\x1b[2J\x1b[Habcd");
    view.pane_images.insert(
        initial.pane_id,
        vec![AttachPaneImage {
            id: 1,
            protocol: AttachImageProtocol::KittyGraphics,
            raw_data: vec![
                137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0,
                2, 8, 6, 0, 0, 0, 114, 182, 13, 36, 0, 0, 0, 18, 73, 68, 65, 84, 120, 156, 99, 248,
                207, 192, 0, 66, 64, 12, 36, 64, 44, 0, 67, 206, 7, 249, 19, 116, 7, 229, 0, 0, 0,
                0, 73, 69, 78, 68, 174, 66, 96, 130,
            ],
            compression: CompressionId::None,
            pixel_width: 2,
            pixel_height: 2,
            cell_cols: 1,
            cell_rows: 1,
            position_col: 0,
            position_row: 0,
        }],
    );
    render(&mut view, &layout);
    let target = handle.pane_zoom_target(None).unwrap();
    let mut client = disconnected_focus_test_client().await;
    super::super::pane_zoom_native::set_target(&mut view, &mut client, target, 1500)
        .await
        .unwrap();
    render(&mut view, &layout);
    assert_eq!(
        handle
            .refresh_pane_zoom_target(&target)
            .unwrap()
            .scale_millis,
        1500
    );
    let grids = view.presentation.grid_fragments();
    let local = grids
        .iter()
        .find(|grid| grid.id().starts_with("pane-zoom:"))
        .unwrap();
    assert_eq!(local.images().len(), 1);
    assert_eq!(
        local.images()[0].destination,
        bmux_tui::geometry::Rect::new(0, 0, 1, 1)
    );
}

#[tokio::test]
async fn pane_zoom_uses_committed_target_and_keeps_replica_geometry() {
    let (mut view, handle, _host, layout) = fixture();
    let target = handle
        .pane_zoom_target(None)
        .expect("committed focused pane");
    let before = view.pane_buffers[&target.pane_id]
        .terminal_grid
        .grid()
        .snapshot(0, 24);
    let mut client = disconnected_focus_test_client().await;
    super::super::pane_zoom_native::set_target(&mut view, &mut client, target, 2000)
        .await
        .unwrap();
    // Intent is not visible to the handle until complete delivery.
    assert_eq!(handle.pane_zoom_target(None), Some(target));
    render(&mut view, &layout);
    let committed = handle.pane_zoom_target(None).unwrap();
    assert_eq!(committed.scale_millis, 2000);
    assert_ne!(committed.identity, target.identity);
    assert_eq!(
        view.pane_buffers[&target.pane_id]
            .terminal_grid
            .grid()
            .snapshot(0, 24),
        before
    );
    assert_ne!(view.presentation.grid_fragments(), []);
    assert!(
        view.pane_buffers[&target.pane_id]
            .local_zoom
            .preserve_execution_geometry
    );
    super::super::pane_zoom_native::set_target(&mut view, &mut client, target, 3000)
        .await
        .unwrap();
    assert_eq!(view.pane_zoom.requested[&target.view_id].1, 2000);
}

#[tokio::test]
async fn pane_zoom_failed_delivery_retains_query_and_mapping_authority() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let fixture = attach_view_state_with_scrollback_fixture();
    let layout = fixture.cached_layout_state.clone().unwrap();
    let rejected = Arc::new(AtomicBool::new(false));
    let reject = rejected.clone();
    let (handle, mut host) = NativeAttachHost::channel(80, 24, 7, move |_| {
        anyhow::ensure!(
            !reject.load(Ordering::Acquire),
            "injected zoom delivery failure"
        );
        Ok(())
    })
    .unwrap();
    let mut view = AttachViewState::with_private_presentation(bmux_client::AttachOpenInfo {
        context_id: None,
        session_id: layout.session_id,
        can_write: true,
    });
    view.pane_buffers = fixture.pane_buffers;
    // Test-only authoritative capture-source token fixture; production obtains
    // this through capture_source before/after structured snapshot hydration.
    for pane in view.pane_buffers.keys() {
        view.pane_zoom
            .execution_tokens
            .insert(*pane, Uuid::new_v4());
    }
    view.presentation_only = true;
    view.set_observer(host.observer.take().unwrap());
    render(&mut view, &layout);
    let target = handle.pane_zoom_target(None).unwrap();
    let mut client = disconnected_focus_test_client().await;
    super::super::pane_zoom_native::set_target(&mut view, &mut client, target, 2000)
        .await
        .unwrap();
    rejected.store(true, Ordering::Release);
    render(&mut view, &layout); // Browser rejection is recoverable, never commit.
    assert_eq!(handle.pane_zoom_target(None), Some(target));
    assert!(view.pane_zoom.committed.is_empty());
    rejected.store(false, Ordering::Release);
    render(&mut view, &layout);
    assert_eq!(handle.pane_zoom_target(None).unwrap().scale_millis, 2000);
    assert!(!view.pane_zoom.committed.is_empty());
}

#[tokio::test]
async fn pane_zoom_selection_uses_logical_cells_and_rejects_changed_source() {
    let (mut view, handle, _host, layout) = fixture();
    let initial = handle.pane_zoom_target(None).unwrap();
    let grid = &mut view
        .pane_buffers
        .get_mut(&initial.pane_id)
        .unwrap()
        .terminal_grid;
    grid.process("\x1b[2J\x1b[Hab界cd".as_bytes());
    render(&mut view, &layout);
    let target = handle.pane_zoom_target(None).unwrap();
    let mut client = disconnected_focus_test_client().await;
    super::super::pane_zoom_native::set_target(&mut view, &mut client, target, 2000)
        .await
        .unwrap();
    render(&mut view, &layout);
    let target = handle.pane_zoom_target(None).unwrap();
    let surface = view.retained_compositor.surfaces()[&target.view_id].clone();
    let transform = surface.content_transform.unwrap();
    let event = |kind| crossterm::event::MouseEvent {
        kind,
        column: surface.rect.x,
        row: surface.rect.y,
        modifiers: crossterm::event::KeyModifiers::empty(),
    };
    // The fixed two-row allocation displays the reflowed tail: the wide glyph
    // begins at local column zero, despite execution column two.
    view.pane_zoom.shared_pointer_root = Some(transform.project([0.25, 0.25]));
    assert!(super::super::pane_zoom_native::begin_selection(
        &mut view,
        target.pane_id,
        event(MouseEventKind::Down(MouseButton::Left))
    ));
    view.pane_zoom.shared_pointer_root = Some(transform.project([1.25, 0.25]));
    assert!(super::super::pane_zoom_native::drag_selection(
        &mut view,
        event(MouseEventKind::Up(MouseButton::Left))
    ));
    assert_eq!(
        super::super::pane_zoom_native::selected_text(&view).as_deref(),
        Some("界")
    );
    view.pane_buffers
        .get_mut(&target.pane_id)
        .unwrap()
        .terminal_grid
        .process(b"!");
    assert!(
        super::super::pane_zoom_native::execution_at(
            &view,
            transform.project([2.25, 0.25]),
            target.pane_id
        )
        .is_none()
    );
    render(&mut view, &layout);
    assert!(view.pane_zoom.selection.is_none());
}

#[test]
fn fractional_native_pointer_is_preserved_until_retained_inverse() {
    let (view, handle, _host, _layout) = fixture();
    let target = handle.pane_zoom_target(None).unwrap();
    assert!(handle.pane_zoom_target(Some([f64::NAN, 0.0])).is_none());
    let event = super::super::pane_zoom_native::expand_pointer(
        target.identity,
        [1.75, 2.25],
        NativePanePointer::Motion {
            drag: None,
            modifiers: Modifiers::default(),
        },
    )
    .unwrap();
    assert!(matches!(
        event,
        NativeAttachEvent::Motion {
            column: 1,
            row: 2,
            ..
        }
    ));
    assert!(view.pane_zoom.shared.is_some());
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

async fn reduce_paste(
    view: &mut AttachViewState,
    client: &mut StreamingBmuxClient,
    text: &str,
    normalized: bool,
) -> Result<AttachLoopControl> {
    let raw = Event::Paste(text.into());
    let mut event = AttachTerminalEvent::from_crossterm(
        raw,
        Instant::now(),
        TerminalGeometry { cols: 80, rows: 24 },
    );
    if !normalized {
        event.normalized = None;
    }
    let mut processor =
        InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
    handle_attach_terminal_event(
        client,
        event,
        &mut processor,
        &[],
        view,
        &mut DisplayCaptureFanout::default(),
        None,
    )
    .await
}

#[tokio::test]
async fn native_paste_reaches_shared_reducer_without_outer_capture() {
    for requested in [false, true] {
        let mut view = attach_view_state_with_scrollback_fixture();
        view.outer_paste_capture_enabled = false;
        view.can_write = true;
        let pane = focused_attach_pane_id(&view).unwrap();
        view.pane_input_mode_hints.insert(
            pane,
            bmux_attach_layout_protocol::AttachInputModeState {
                bracketed_paste: requested,
                ..Default::default()
            },
        );
        let text = "native\n世界";
        assert_eq!(
            pane_paste_bytes(&view, text).unwrap(),
            encode_bracketed_paste(text, requested)
        );
        let mut client = disconnected_focus_test_client().await;
        // A disconnected sink must report the attempted send, not silently drop
        // an event because capture is disabled. The same path enforces read-only.
        assert!(
            reduce_paste(&mut view, &mut client, text, true)
                .await
                .is_err()
        );
        view.can_write = false;
        assert!(
            reduce_paste(&mut view, &mut client, text, true)
                .await
                .is_ok()
        );
    }
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
    view.outer_paste_capture_enabled = false;
    for normalized in [false, true] {
        reduce_paste(&mut view, &mut client, "clipboard\n世界", normalized)
            .await
            .unwrap();
    }
    assert_eq!(received.lock().unwrap().len(), 3);
    // An unconsumed edit in a modal scope cannot fall through to a pane.
    view.presentation_input.register_paste(
        endpoint,
        Arc::new(|_, _| bmux_plugin::AttachInputResult::default()),
    );
    native_committed_text(&mut client, &mut view, "modal-blocked", true)
        .await
        .unwrap();
    for normalized in [false, true] {
        reduce_paste(&mut view, &mut client, "modal-paste-blocked", normalized)
            .await
            .unwrap();
    }
    view.help_overlay_open = true;
    assert!(native_capture_committed_text(&mut view, "blocked"));
    assert_eq!(received.lock().unwrap().len(), 3);
}
