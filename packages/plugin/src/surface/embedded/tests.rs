use super::*;
use bmux_tui::{
    damage::Damage,
    geometry::Point,
    hit::HitRole,
    presentation::PresentationState,
    selection::{SelectionFragment, SelectionScope},
    semantic::SemanticRegion,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

fn id() -> PluginSurfaceId {
    PluginSurfaceId::new("example.frontend", "view", uuid::Uuid::from_u128(1))
}

/// A 4x2 output: one cell, a focusable field, a label, a selectable fragment, a cursor
/// and a focus scope.
fn snapshot(revision: u64) -> EmbeddedSurface {
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
    let text = BTreeMap::from([(SelectionContentId::new("content"), "abc".to_owned())]);
    EmbeddedSurface::from_frame(id(), revision, &frame, text)
}

/// Paint `paint` into a fresh `width`x`height` producer frame and capture it.
fn captured(
    width: u16,
    height: u16,
    revision: u64,
    paint: impl FnOnce(&mut PaintCx<'_, '_>),
) -> (Buffer, EmbeddedSurface) {
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let surface = {
        let mut frame = Frame::new(&mut buffer);
        paint(&mut PaintCx::new(&mut frame));
        EmbeddedSurface::from_frame(id(), revision, &frame, BTreeMap::new())
    };
    (buffer, surface)
}

fn host(owner: &str) -> EmbeddedSurfaceHost {
    EmbeddedSurfaceHost::new(owner.into(), 7, EmbeddedSurfaceLimits::default()).unwrap()
}

fn publish(
    host: &EmbeddedSurfaceHost,
    generation: u64,
    revision: u64,
    surface: EmbeddedSurface,
) -> EmbeddedResourceId {
    let port = host.producer();
    let EmbeddedSurfaceResponse::Created { resource } = port
        .apply(EmbeddedSurfaceRequest::Create { generation })
        .unwrap()
    else {
        panic!("create answers with a resource");
    };
    port.apply(EmbeddedSurfaceRequest::Publish {
        resource,
        generation,
        revision,
        surface: Box::new(surface),
    })
    .unwrap();
    resource
}

/// Replay `prepared` at (`x`, `y`) in a `child` box of a host frame of `area`, every
/// cell of which first has background `host_bg`.
fn replay_at(
    prepared: &PreparedEmbeddedSurface,
    area: Rect,
    x: i32,
    y: i64,
    child: LocalRect,
    host_bg: Color,
) -> PreparedPresentation {
    PresentationState::new(area)
        .prepare(Damage::Full, |cx| {
            cx.fill(LocalRect::terminal(area), " ", Style::new().bg(host_bg));
            cx.with_child(x, y, child, |cx| prepared.paint(cx));
        })
        .unwrap()
}

fn region_named<'a>(
    replay: &PreparedEmbeddedSurface,
    prepared: &'a PreparedPresentation,
    name: &str,
) -> &'a HitRegion {
    prepared
        .hits()
        .regions()
        .iter()
        .find(|region| {
            replay
                .local_hit(&region.id)
                .is_some_and(|hit| hit.local_id == name)
        })
        .unwrap()
}

#[test]
fn replay_uses_the_host_transform_for_cells_and_all_metadata() {
    let host = host("owner");
    let resource = publish(&host, 2, 1, snapshot(1));
    let replay = host.prepare(resource, 1).unwrap();
    let mut prepared = replay_at(
        &replay,
        Rect::new(0, 0, 12, 6),
        3,
        2,
        LocalRect::new(0, 0, 4, 2),
        Color::Default,
    );
    prepared
        .set_selection_text(replay.selection_text())
        .unwrap();
    assert_eq!(prepared.buffer().get(Point::new(3, 2)).unwrap().symbol, "a");
    let field = region_named(&replay, &prepared, "field");
    assert_eq!(field.area, Rect::new(3, 2, 2, 1));
    assert!(field.focusable);
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
fn captured_cells_replay_exactly_for_wide_combining_and_styled_text() {
    let bold_red = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
    let concealed = Style::new().add_modifier(Modifier::HIDDEN);
    let rgb = Style::new().fg(Color::Rgb(1, 2, 3)).bg(Color::Indexed(7));
    let (producer, surface) = captured(10, 3, 1, |cx| {
        cx.write_line(
            LocalRect::new(0, 0, 10, 1),
            &Line::from_spans(vec![
                Span::styled("漢字", bold_red),
                Span::raw("e\u{301}🙂"),
            ]),
        );
        cx.write_line(
            LocalRect::new(0, 1, 10, 1),
            &Line::from_spans(vec![
                Span::styled("pw", concealed),
                Span::styled("rgb", rgb),
            ]),
        );
    });
    let host = host("owner");
    let resource = publish(&host, 1, 1, surface);
    let replay = host.prepare(resource, 1).unwrap();
    let prepared = replay_at(
        &replay,
        Rect::new(0, 0, 10, 3),
        0,
        0,
        LocalRect::new(0, 0, 10, 3),
        Color::Default,
    );
    for y in 0..3 {
        for x in 0..10 {
            let point = Point::new(x, y);
            let expected = producer.get(point).unwrap();
            let actual = prepared.buffer().get(point).unwrap();
            assert_eq!(actual.symbol, expected.symbol, "symbol at {point:?}");
            assert_eq!(
                actual.is_wide_continuation(),
                expected.is_wide_continuation(),
                "continuation at {point:?}"
            );
            assert_eq!(actual.style.fg, expected.style.fg, "fg at {point:?}");
            assert_eq!(
                actual.style.modifiers, expected.style.modifiers,
                "modifiers at {point:?}"
            );
        }
    }
    assert_eq!(
        prepared.buffer().get(Point::new(2, 1)).unwrap().style.bg,
        Some(Color::Indexed(7))
    );
}

#[test]
fn unstyled_output_keeps_the_host_background_beneath_it() {
    let (_, surface) = captured(3, 1, 1, |cx| cx.set_cell(0, 0, "x", Style::new()));
    let host = host("owner");
    let resource = publish(&host, 1, 1, surface);
    let replay = host.prepare(resource, 1).unwrap();
    let panel = Color::Rgb(9, 9, 9);
    let prepared = replay_at(
        &replay,
        Rect::new(0, 0, 3, 1),
        0,
        0,
        LocalRect::new(0, 0, 3, 1),
        panel,
    );
    let cell = prepared.buffer().get(Point::new(0, 0)).unwrap();
    assert_eq!(cell.symbol, "x");
    assert_eq!(cell.style.bg, Some(panel));
}

#[test]
fn clipping_never_paints_half_a_wide_symbol_or_outside_the_box() {
    let (_, surface) = captured(4, 2, 1, |cx| {
        cx.write_line(LocalRect::new(0, 0, 4, 1), &Line::raw("a漢b"));
        cx.set_cell(0, 1, "z", Style::new());
    });
    let host = host("owner");
    let resource = publish(&host, 1, 1, surface);
    let replay = host.prepare(resource, 1).unwrap();
    // The box is two columns and one row: "a" fits, the wide symbol does not.
    let prepared = replay_at(
        &replay,
        Rect::new(0, 0, 6, 3),
        1,
        1,
        LocalRect::new(0, 0, 2, 1),
        Color::Default,
    );
    let buffer = prepared.buffer();
    assert_eq!(buffer.get(Point::new(1, 1)).unwrap().symbol, "a");
    assert_eq!(buffer.get(Point::new(2, 1)).unwrap().symbol, " ");
    assert_eq!(buffer.get(Point::new(3, 1)).unwrap().symbol, " ");
    assert_eq!(buffer.get(Point::new(1, 2)).unwrap().symbol, " ");
}

#[test]
fn replayed_regions_cover_exactly_the_cells_the_producer_declared() {
    let (_, surface) = captured(6, 2, 1, |cx| {
        cx.write_line(LocalRect::new(0, 0, 6, 1), &Line::raw("[ok]"));
        cx.push_hit(
            HitRegion::new("ok", Rect::new(0, 0, 4, 1))
                .role(HitRole::Action)
                .focusable(true),
        );
        cx.push_hit(HitRegion::new("hidden", Rect::new(0, 1, 2, 1)).visible(false));
    });
    let host = host("owner");
    let resource = publish(&host, 1, 1, surface);
    let replay = host.prepare(resource, 1).unwrap();
    let prepared = replay_at(
        &replay,
        Rect::new(0, 0, 10, 4),
        2,
        1,
        LocalRect::new(0, 0, 6, 2),
        Color::Default,
    );
    let ok = region_named(&replay, &prepared, "ok");
    assert_eq!(ok.area, Rect::new(2, 1, 4, 1));
    assert_eq!(ok.role, HitRole::Action);
    assert!(ok.focusable && ok.enabled);
    for x in 2..6 {
        assert_ne!(prepared.buffer().get(Point::new(x, 1)).unwrap().symbol, " ");
    }
    assert!(!region_named(&replay, &prepared, "hidden").visible);
}

#[test]
fn publication_and_failed_display_never_admit_input_to_undisplayed_output() {
    let host = host("owner");
    let resource = publish(&host, 2, 10, snapshot(10));
    let port = host.producer();
    assert_eq!(
        host.validate_input(resource, 2, 10),
        Err(EmbeddedSurfaceError::NotCommitted)
    );
    host.acknowledge(resource, 2, 10).unwrap();
    port.apply(EmbeddedSurfaceRequest::Publish {
        resource,
        generation: 2,
        revision: 11,
        surface: Box::new(snapshot(11)),
    })
    .unwrap();
    host.validate_input(resource, 2, 10).unwrap();
    assert_eq!(
        host.validate_input(resource, 2, 11),
        Err(EmbeddedSurfaceError::Stale)
    );
    host.display_failed().unwrap();
    assert_eq!(
        host.validate_input(resource, 2, 10),
        Err(EmbeddedSurfaceError::NotCommitted)
    );
    host.acknowledge(resource, 2, 11).unwrap();
    host.validate_input(resource, 2, 11).unwrap();
    assert_eq!(
        host.acknowledge(resource, 2, 10),
        Err(EmbeddedSurfaceError::Stale)
    );
    for (generation, revision) in [(2, 11), (3, 12)] {
        assert_eq!(
            port.apply(EmbeddedSurfaceRequest::Publish {
                resource,
                generation,
                revision,
                surface: Box::new(snapshot(revision)),
            }),
            Err(EmbeddedSurfaceError::Stale)
        );
    }
}

#[test]
fn a_newer_display_revokes_input_snapshots_of_the_older_one() {
    let host = host("owner");
    let resource = publish(&host, 1, 1, snapshot(1));
    host.acknowledge(resource, 1, 1).unwrap();
    let old = host.committed_for_input(resource, 1, 1).unwrap();
    assert!(old.local_hit(&HitId::new(old.scoped("field"))).is_some());
    host.producer()
        .apply(EmbeddedSurfaceRequest::Publish {
            resource,
            generation: 1,
            revision: 2,
            surface: Box::new(snapshot(2)),
        })
        .unwrap();
    host.acknowledge(resource, 1, 2).unwrap();
    assert!(old.local_hit(&HitId::new(old.scoped("field"))).is_none());
    assert_eq!(old.selection_text(), BTreeMap::new());
}

#[test]
fn destroyed_resources_and_dropped_hosts_paint_and_accept_nothing() {
    let host = host("owner");
    let resource = publish(&host, 1, 1, snapshot(1));
    let replay = host.prepare(resource, 1).unwrap();
    let port = host.producer();
    port.apply(EmbeddedSurfaceRequest::Destroy { resource })
        .unwrap();
    let prepared = replay_at(
        &replay,
        Rect::new(0, 0, 4, 2),
        0,
        0,
        LocalRect::new(0, 0, 4, 2),
        Color::Default,
    );
    assert_eq!(prepared.buffer().get(Point::new(0, 0)).unwrap().symbol, " ");
    assert_eq!(prepared.hits().regions(), &[]);
    drop(host);
    assert_eq!(
        port.apply(EmbeddedSurfaceRequest::Create { generation: 1 }),
        Err(EmbeddedSurfaceError::Unavailable)
    );
}

#[test]
fn separate_activations_never_share_identities() {
    let first = host("first");
    let second = host("second");
    let a = publish(&first, 1, 1, snapshot(1));
    let b = publish(&second, 1, 1, snapshot(1));
    let a = first.prepare(a, 1).unwrap();
    let b = second.prepare(b, 1).unwrap();
    assert_ne!(a.scoped("field"), b.scoped("field"));
    assert!(a.local_hit(&HitId::new(b.scoped("field"))).is_none());
    let recreated =
        EmbeddedSurfaceHost::new("first".into(), 8, EmbeddedSurfaceLimits::default()).unwrap();
    let c = publish(&recreated, 1, 1, snapshot(1));
    assert_ne!(
        a.scoped("field"),
        recreated.prepare(c, 1).unwrap().scoped("field")
    );
}

#[test]
fn budgets_bound_cells_pending_revisions_and_resources() {
    let limits = EmbeddedSurfaceLimits {
        max_cells: 4,
        max_resources: 1,
        max_pending: 1,
        ..EmbeddedSurfaceLimits::default()
    };
    let host = EmbeddedSurfaceHost::new("owner".into(), 1, limits).unwrap();
    let port = host.producer();
    let EmbeddedSurfaceResponse::Created { resource } = port
        .apply(EmbeddedSurfaceRequest::Create { generation: 1 })
        .unwrap()
    else {
        panic!("create answers with a resource");
    };
    assert_eq!(
        port.apply(EmbeddedSurfaceRequest::Create { generation: 1 }),
        Err(EmbeddedSurfaceError::Limit)
    );
    let (_, large) = captured(3, 2, 1, |_| {});
    assert_eq!(
        port.apply(EmbeddedSurfaceRequest::Publish {
            resource,
            generation: 1,
            revision: 1,
            surface: Box::new(large),
        }),
        Err(EmbeddedSurfaceError::Limit)
    );
    let small = |revision| captured(2, 2, revision, |_| {}).1;
    port.apply(EmbeddedSurfaceRequest::Publish {
        resource,
        generation: 1,
        revision: 2,
        surface: Box::new(small(2)),
    })
    .unwrap();
    assert_eq!(
        port.apply(EmbeddedSurfaceRequest::Publish {
            resource,
            generation: 1,
            revision: 3,
            surface: Box::new(small(3)),
        }),
        Err(EmbeddedSurfaceError::Limit)
    );
}

#[test]
fn frame_state_outside_the_output_is_rejected() {
    let limits = EmbeddedSurfaceLimits::default();
    let mut surface = snapshot(1);
    surface.surface.interactive_regions[0].rect = ExtensionRect::new(3, 0, 2, 1);
    assert_eq!(surface.validate(limits), Err(EmbeddedSurfaceError::Invalid));
    let mut surface = snapshot(1);
    surface.frame.cursor = Some(Cursor {
        position: Point::new(0, 2),
        visible: true,
    });
    assert_eq!(surface.validate(limits), Err(EmbeddedSurfaceError::Invalid));
    let mut surface = snapshot(1);
    surface.frame.modal_scopes = vec![HitId::new("same"), HitId::new("same")];
    assert_eq!(surface.validate(limits), Err(EmbeddedSurfaceError::Invalid));
}

#[test]
fn embedded_and_screen_targets_stay_in_their_own_hosts() {
    let mut surface = snapshot(1);
    surface.surface.target = PluginSurfaceTarget::Explicit(ExtensionRect::new(0, 0, 4, 2));
    assert!(matches!(
        surface.validate(EmbeddedSurfaceLimits::default()),
        Err(EmbeddedSurfaceError::Rejected(error))
            if matches!(*error, PluginSurfacePublishError::NotEmbedded { .. })
    ));
    let registry = super::super::PluginSurfaceRegistry::new(4);
    assert!(matches!(
        registry.publish(
            "example.frontend",
            PluginSurfaceSnapshot {
                revision: 1,
                surfaces: vec![snapshot(1).surface],
            }
        ),
        Err(PluginSurfacePublishError::EmbeddedTargetOnScreen { .. })
    ));
}

#[test]
fn selection_is_stamped_with_the_published_revision() {
    let surface = snapshot(42);
    let selection = surface
        .surface
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.selection.as_ref())
        .unwrap();
    assert!(selection.scopes().iter().all(|scope| scope.revision == 42));
    assert!(
        selection
            .fragments()
            .iter()
            .all(|fragment| fragment.revision == 42)
    );
    surface.validate(EmbeddedSurfaceLimits::default()).unwrap();
}

#[test]
fn the_wire_contract_round_trips_and_rejects_other_contracts_and_oversized_payloads() {
    let limits = EmbeddedSurfaceLimits::default();
    let surface = snapshot(1);
    let decoded = EmbeddedSurface::decode(&surface.encode(limits).unwrap(), limits).unwrap();
    assert_eq!(decoded, surface);
    let host = host("owner");
    let port = host.producer();
    let create = serde_json::to_vec(&EmbeddedSurfaceRequest::Create { generation: 1 }).unwrap();
    assert_eq!(
        port.extension("bmux.presentation.retained.v2", &create),
        Err(EmbeddedSurfaceError::Contract)
    );
    assert!(matches!(
        port.extension(EMBEDDED_SURFACE_CONTRACT_ID, &create),
        Ok(EmbeddedSurfaceResponse::Created { .. })
    ));
    assert_eq!(
        port.extension(EMBEDDED_SURFACE_CONTRACT_ID, b"{"),
        Err(EmbeddedSurfaceError::Encoding)
    );
    let tight = EmbeddedSurfaceHost::new(
        "owner".into(),
        1,
        EmbeddedSurfaceLimits {
            max_bytes: 4,
            ..limits
        },
    )
    .unwrap();
    assert_eq!(
        tight
            .producer()
            .extension(EMBEDDED_SURFACE_CONTRACT_ID, &create),
        Err(EmbeddedSurfaceError::Limit)
    );
}

#[test]
fn empty_and_single_cell_outputs_replay_without_painting_outside() {
    let (_, empty) = captured(0, 0, 1, |_| {});
    let host = host("owner");
    let resource = publish(&host, 1, 1, empty);
    let replay = host.prepare(resource, 1).unwrap();
    let prepared = replay_at(
        &replay,
        Rect::new(0, 0, 2, 1),
        0,
        0,
        LocalRect::new(0, 0, 2, 1),
        Color::Default,
    );
    assert_eq!(prepared.buffer().get(Point::new(0, 0)).unwrap().symbol, " ");
    let (_, single) = captured(1, 1, 1, |cx| cx.set_cell(0, 0, "q", Style::new()));
    let resource = publish(&host, 1, 1, single);
    let replay = host.prepare(resource, 1).unwrap();
    let prepared = replay_at(
        &replay,
        Rect::new(0, 0, 3, 1),
        1,
        0,
        LocalRect::new(0, 0, 1, 1),
        Color::Default,
    );
    assert_eq!(prepared.buffer().get(Point::new(1, 0)).unwrap().symbol, "q");
    assert_eq!(prepared.buffer().get(Point::new(2, 0)).unwrap().symbol, " ");
}
