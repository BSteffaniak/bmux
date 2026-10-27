//! Exercises the actual native host receipt and retained render transaction, without a GUI.
use super::tests::attach_view_state_with_scrollback_fixture;
use super::*;
use bmux_client::AttachOpenInfo;
use bmux_plugin::RenderStyle;
use bmux_plugin::surface::{
    PluginSurface, PluginSurfaceId, PluginSurfaceMetadata, PluginSurfaceRegion,
    PluginSurfaceSnapshot, PluginSurfaceTarget,
};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::selection::{SelectionFragment, SelectionScene, SelectionScope};
use bmux_tui::semantic::{SemanticRegion, SemanticScene};
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn stationary_pointer_follows_only_successfully_delivered_native_scene() {
    use bmux_attach_pipeline::pointer_shape::ResolvedPointerShape;
    use bmux_terminal_protocol::pointer_shape::PointerShape;
    let fixture = attach_view_state_with_scrollback_fixture();
    let layout = fixture.cached_layout_state.clone().unwrap();
    let mut view = AttachViewState::with_private_presentation(AttachOpenInfo {
        context_id: None,
        session_id: layout.session_id,
        can_write: true,
    });
    view.pane_buffers = fixture.pane_buffers;
    view.presentation_only = true;
    view.mouse.config.pointer_shapes = true;
    view.mouse.last_position = Some((1, 0));
    let rejected = Arc::new(AtomicBool::new(false));
    let reject = Arc::clone(&rejected);
    let pointers = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&pointers);
    let publisher = super::super::observer::AttachObserverPublisher::for_prepared_delivery(
        1,
        move |delivery| {
            anyhow::ensure!(!reject.load(Ordering::SeqCst), "injected failure");
            recorded.lock().unwrap().push(delivery.pointer_shape);
            Ok(())
        },
    );
    view.set_observer(publisher);
    publish(&view, 1, 1);
    render(&mut view, &layout);
    let text = Some(ResolvedPointerShape::Shape(PointerShape::Text));
    assert_eq!(view.last_pointer_shape, text);
    assert_eq!(pointers.lock().unwrap().last(), Some(&text));
    let graph = view.retained_compositor.clone();
    rejected.store(true, Ordering::SeqCst);
    publish_with_cursor(
        &view,
        2,
        1,
        bmux_plugin::surface::PluginSurfaceCursor::Pointer,
    );
    view.dirty
        .mark_retained_surfaces_dirty(AttachDirtySource::SceneChanged);
    render(&mut view, &layout);
    assert_eq!(view.retained_compositor, graph);
    assert_eq!(view.last_pointer_shape, text);
    assert_eq!(pointers.lock().unwrap().last(), Some(&text));
    rejected.store(false, Ordering::SeqCst);
    // Rejected output deliberately suppresses automatic repaint. Model the
    // generation-matched native retry event rather than calling an idle render.
    request_native_presentation_retry(&mut view, Some(1), 1);
    render(&mut view, &layout);
    assert_eq!(
        view.last_pointer_shape,
        Some(ResolvedPointerShape::Shape(PointerShape::Pointer))
    );
    assert_eq!(
        pointers.lock().unwrap().last(),
        Some(&view.last_pointer_shape)
    );
}

#[test]
fn failed_pointer_only_delivery_retries_the_committed_scene_without_false_cache_commit() {
    use bmux_attach_pipeline::pointer_shape::ResolvedPointerShape;
    use bmux_terminal_protocol::pointer_shape::PointerShape;
    let fixture = attach_view_state_with_scrollback_fixture();
    let layout = fixture.cached_layout_state.clone().unwrap();
    let mut view = AttachViewState::with_private_presentation(AttachOpenInfo {
        context_id: None,
        session_id: layout.session_id,
        can_write: true,
    });
    view.pane_buffers = fixture.pane_buffers;
    view.presentation_only = true;
    view.mouse.config.pointer_shapes = true;
    view.mouse.last_position = Some((1, 0));
    let pointer = Some(ResolvedPointerShape::Shape(PointerShape::Pointer));
    let text = Some(ResolvedPointerShape::Shape(PointerShape::Text));
    let rejected = Arc::new(AtomicBool::new(true));
    let reject = Arc::clone(&rejected);
    let deliveries = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&deliveries);
    view.set_observer(
        super::super::observer::AttachObserverPublisher::for_prepared_delivery(
            1,
            move |delivery| {
                anyhow::ensure!(
                    delivery.pointer_shape != pointer || !reject.load(Ordering::SeqCst),
                    "injected pointer-only failure"
                );
                recorded.lock().unwrap().push(delivery);
                Ok(())
            },
        ),
    );
    publish(&view, 1, 1);
    render(&mut view, &layout);
    publish_with_cursor(
        &view,
        2,
        1,
        bmux_plugin::surface::PluginSurfaceCursor::Pointer,
    );
    view.dirty
        .mark_retained_surfaces_dirty(AttachDirtySource::SceneChanged);
    assert!(
        render_attach_frame_to_output(
            None,
            &mut view,
            &layout,
            &RuntimeAppearance::default(),
            &[],
            0,
            &bmux_config::DamageBehaviorConfig::default(),
            u64::MAX,
            &mut DisplayCaptureFanout::default(),
            TerminalGeometry { cols: 80, rows: 24 },
            None,
        )
        .is_err()
    );
    assert_eq!(view.pointer_shape, pointer);
    assert_eq!(view.last_pointer_shape, text);
    let graph = view.retained_compositor.clone();
    let committed = deliveries.lock().unwrap().last().unwrap().clone();
    rejected.store(false, Ordering::SeqCst);
    present_committed_pointer_shape(&mut view).unwrap();
    assert_eq!(view.last_pointer_shape, pointer);
    assert_eq!(view.retained_compositor, graph);
    let retried = deliveries.lock().unwrap().last().unwrap().clone();
    assert_eq!(retried.pointer_shape, pointer);
    assert_eq!(retried.prepared.buffer(), committed.prepared.buffer());
    assert_eq!(retried.prepared.selection(), committed.prepared.selection());
    assert_eq!(retried.prepared.semantics(), committed.prepared.semantics());
    assert!(retried.identity.revision > committed.identity.revision);
}

fn publish(view: &AttachViewState, revision: u64, x: u16) {
    publish_with_cursor(
        view,
        revision,
        x,
        bmux_plugin::surface::PluginSurfaceCursor::Text,
    );
}

fn publish_with_cursor(
    view: &AttachViewState,
    revision: u64,
    x: u16,
    cursor: bmux_plugin::surface::PluginSurfaceCursor,
) {
    let area = Rect::new(0, 0, 4, 1);
    let mut selection = SelectionScene::new();
    selection.push_scope(SelectionScope::new("scope", area).revision(revision));
    selection
        .push_fragment(SelectionFragment::new("scope", "text", area, 0, 0..4).revision(revision));
    let mut semantics = SemanticScene::new();
    semantics.push(SemanticRegion::new("field", area, "textbox"));
    let mut surface = PluginSurface::layout(
        PluginSurfaceId::new("test.native-retry", "strip", Uuid::from_u128(701)),
        revision,
        bmux_plugin::layout::PluginLayoutId::new("test.native-retry", "layout"),
        vec![RenderOp::text_run(0, 0, "text", RenderStyle::new())],
    )
    .interactive_region(
        PluginSurfaceRegion::new("field", ExtensionRect::new(0, 0, 4, 1))
            .endpoint(bmux_plugin::AttachInputEndpoint {
                capability: "test.native-retry".into(),
                interface_id: "input".into(),
                operation: "handle".into(),
            })
            .focusable(cursor),
    );
    surface.layer = 100;
    surface.target = PluginSurfaceTarget::Explicit(ExtensionRect::new(x, 0, 4, 1));
    surface.metadata = Some(PluginSurfaceMetadata {
        selection: Some(selection),
        semantics: Some(semantics),
        text: BTreeMap::from([("text".into(), "text".into())]),
    });
    view.presentation_surfaces
        .publish(
            "test.native-retry",
            PluginSurfaceSnapshot {
                revision,
                surfaces: vec![surface],
            },
        )
        .unwrap();
}

fn render(view: &mut AttachViewState, layout: &AttachLayoutState) {
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

fn committed_fixture(view: &mut AttachViewState, layout: &AttachLayoutState) {
    publish(view, 1, 1);
    render(view, layout);
    let hit = view.retained_compositor.hit_test(1, 0).unwrap();
    view.plugin_focus.focus_hit(&view.retained_compositor, &hit);
    view.dirty.mark_full_frame(AttachDirtySource::ManualRedraw);
    render(view, layout);
    assert!(view.presentation.focus().active().is_some());
    assert!(
        view.presentation
            .hits()
            .hit_test(Point::new(1, 0))
            .is_some()
    );
    assert_ne!(view.presentation.selection().fragments(), []);
    assert_ne!(view.presentation.semantics().regions(), []);
}

fn assert_attempts(
    attempts: &[bmux_frontend_runtime::presentation::PresentationIdentity],
    epoch: Uuid,
) {
    assert_eq!(
        attempts.iter().map(|id| id.revision).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert!(
        attempts
            .iter()
            .all(|id| id.viewport_generation == 7 && id.epoch == epoch)
    );
}

fn assert_same_retried_scene(scenes: &[Arc<bmux_tui::presentation::PreparedPresentation>]) {
    assert_eq!(scenes[2].buffer(), scenes[3].buffer());
    assert_eq!(scenes[2].selection(), scenes[3].selection());
    assert_eq!(scenes[2].semantics(), scenes[3].semantics());
}

#[test]
fn native_rejection_retains_graph_hits_focus_and_metadata_until_fresh_retry() {
    let fixture = attach_view_state_with_scrollback_fixture();
    let layout = fixture.cached_layout_state.clone().unwrap();
    let mut view = AttachViewState::with_private_presentation(AttachOpenInfo {
        context_id: None,
        session_id: layout.session_id,
        can_write: true,
    });
    view.pane_buffers = fixture.pane_buffers;
    view.presentation_only = true;
    let rejected = Arc::new(AtomicBool::new(false));
    let reject = rejected.clone();
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let recorded = attempts.clone();
    let scenes = Arc::new(Mutex::new(Vec::new()));
    let recorded_scenes = scenes.clone();
    let (_handle, mut host) = NativeAttachHost::channel(80, 24, 7, move |delivery| {
        recorded.lock().unwrap().push(delivery.identity);
        recorded_scenes.lock().unwrap().push(delivery.prepared);
        anyhow::ensure!(
            !reject.load(Ordering::SeqCst),
            "injected native surface loss"
        );
        Ok(())
    })
    .unwrap();
    view.set_observer(host.observer.take().unwrap());
    committed_fixture(&mut view, &layout);
    let graph = view.retained_compositor.clone();
    let focus = view.plugin_focus.focused().cloned();
    let identity = view
        .observer
        .as_ref()
        .unwrap()
        .committed_prepared_identity()
        .unwrap();
    let selection = view.presentation.selection().clone();
    let semantics = view.presentation.semantics().clone();
    let cursor = view.presentation.cursor();
    let committed_focus = view.presentation.focus().active().cloned();
    rejected.store(true, Ordering::SeqCst);
    publish(&view, 2, 12);
    view.dirty
        .mark_retained_surfaces_dirty(AttachDirtySource::SceneChanged);
    render(&mut view, &layout);
    assert_eq!(view.retained_compositor, graph);
    assert_eq!(view.plugin_focus.focused(), focus.as_ref());
    assert_eq!(view.presentation.focus().active(), committed_focus.as_ref());
    assert_eq!(view.presentation.selection(), &selection);
    assert_eq!(view.presentation.semantics(), &semantics);
    assert_eq!(view.presentation.cursor(), cursor);
    assert!(
        view.presentation
            .hits()
            .hit_test(Point::new(1, 0))
            .is_some()
    );
    assert!(
        view.presentation
            .hits()
            .hit_test(Point::new(12, 0))
            .is_none()
    );
    assert_eq!(
        view.observer
            .as_ref()
            .unwrap()
            .committed_prepared_identity(),
        Some(identity)
    );
    assert_eq!(host.accepted_revision(), identity.revision);
    rejected.store(false, Ordering::SeqCst);
    // The native retry event marks full damage for this generation; no producer
    // publication or viewport change occurs between rejection and this retry.
    request_native_presentation_retry(&mut view, Some(7), 7);
    render(&mut view, &layout);
    assert!(
        view.presentation
            .hits()
            .hit_test(Point::new(12, 0))
            .is_some()
    );
    assert!(
        view.presentation
            .hits()
            .hit_test(Point::new(1, 0))
            .is_none()
    );
    assert_ne!(view.presentation.selection(), &selection);
    assert_ne!(view.presentation.semantics(), &semantics);
    let attempts = attempts.lock().unwrap();
    assert_attempts(&attempts, identity.epoch);
    assert_eq!(host.accepted_revision(), 4);
    drop(attempts);
    let scenes = scenes.lock().unwrap();
    assert_same_retried_scene(&scenes);
    drop(scenes);
}

#[test]
fn stale_native_retry_cannot_dirty_current_generation() {
    let mut view = attach_view_state_with_scrollback_fixture();
    view.dirty.clear_frame_damage();
    request_native_presentation_retry(&mut view, Some(8), 7);
    assert!(!view.dirty.retained_surfaces_need_reconcile);
    assert!(!view.dirty.full_pane_redraw);
    request_native_presentation_retry(&mut view, Some(8), 8);
    assert!(view.dirty.retained_surfaces_need_reconcile);
    assert!(view.dirty.full_pane_redraw);
}
