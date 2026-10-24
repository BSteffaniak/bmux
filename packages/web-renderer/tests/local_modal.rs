use bmux_tui::{
    damage::Damage,
    event::{Event, MouseButton, MouseEvent, MouseEventKind},
    geometry::{Point, Rect},
    hit::{HitId, HitRegion},
    presentation::PresentationLimits,
};
use bmux_web_renderer::LocalSurface;
fn surface() -> LocalSurface {
    LocalSurface::new(
        Rect::new(0, 0, 4, 2),
        PresentationLimits {
            max_cells: 32,
            max_entries: 128,
            max_bytes: 4096,
        },
    )
}
fn pointer(kind: MouseEventKind, x: u16, y: u16) -> Event {
    Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
}
fn hit(cx: &mut bmux_tui::paint::PaintCx<'_, '_>, id: &str, area: Rect) {
    cx.push_hit(HitRegion::new(id, area).focusable(true));
}

#[test]
fn modal_failure_cannot_install_background_or_speculative_focus() {
    let mut surface = surface();
    surface
        .present(
            Damage::Full,
            |cx| hit(cx, "background", Rect::new(0, 0, 2, 1)),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let modal = HitId::new("modal");
    let paint_modal = |cx: &mut bmux_tui::paint::PaintCx<'_, '_>| {
        cx.set_focus_scope(Some(HitId::new("modal")));
        hit(cx, "inside", Rect::new(2, 0, 2, 1));
    };
    assert!(
        surface
            .present_with_metadata(
                Damage::Full,
                paint_modal,
                |frame| {
                    assert!(frame.set_modal_scopes(std::slice::from_ref(&modal)));
                },
                |_| Err("output")
            )
            .is_err()
    );
    assert!(
        surface
            .route(Event::Paste("x".into()))
            .targets(&HitId::new("background"))
    );
    surface
        .present_with_metadata(
            Damage::Full,
            paint_modal,
            |frame| {
                assert!(frame.set_modal_scopes(std::slice::from_ref(&modal)));
            },
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert!(
        surface
            .route(pointer(MouseEventKind::Down(MouseButton::Left), 0, 0))
            .target
            .is_none()
    );
    surface.cancel_pointer();
    assert!(
        surface
            .route(Event::Paste("x".into()))
            .targets(&HitId::new("inside"))
    );
    surface
        .present(
            Damage::Full,
            |cx| hit(cx, "background", Rect::new(0, 0, 2, 1)),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert!(
        surface
            .route(Event::Paste("x".into()))
            .targets(&HitId::new("background"))
    );
}
