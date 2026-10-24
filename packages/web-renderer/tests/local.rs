use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::{
    damage::Damage,
    event::{Event, MouseButton, MouseEvent, MouseEventKind},
    geometry::{Point, Rect},
    hit::{HitId, HitRegion},
    paint::LocalRect,
    presentation::PresentationLimits,
    style::Style,
};
use bmux_web_renderer::{
    LocalSurface,
    input::{BrowserInput, InputError},
};

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
fn text_is_committed_once_and_payloads_are_bounded() {
    let key = BrowserInput::decode(
        r#"{"type":"key","key":"a","code":"KeyA","repeat":false}"#,
        512,
    )
    .unwrap();
    assert!(key.normalize(32, |_, _| None).unwrap().events.is_empty());
    let text = BrowserInput::decode(r#"{"type":"text","text":"漢字"}"#, 512).unwrap();
    assert_eq!(
        text.clone().normalize(32, |_, _| None).unwrap().events,
        vec![Event::Paste("漢字".into())]
    );
    assert_eq!(text.normalize(1, |_, _| None), Err(InputError::TooLarge));
    assert!(matches!(
        BrowserInput::decode("{}", 1),
        Err(InputError::TooLarge)
    ));
    let shortcut = BrowserInput::decode(
        r#"{"type":"key","key":"A","code":"KeyA","ctrl":true,"meta":true}"#,
        512,
    )
    .unwrap()
    .normalize(32, |_, _| None)
    .unwrap();
    let Event::Key(stroke) = shortcut.events[0] else {
        panic!()
    };
    assert_eq!(stroke.key, KeyCode::Char('a'));
    assert!(stroke.modifiers.ctrl && stroke.modifiers.super_key);
}

#[test]
fn coordinate_mapping_and_wheel_work_are_bounded() {
    let wheel = BrowserInput::decode(r#"{"type":"wheel","x":12.5,"y":25,"delta_x":-100000,"delta_y":100000,"delta_mode":0,"meta":true}"#, 512).unwrap();
    let output = wheel
        .normalize(32, |x, y| {
            assert_eq!((x, y), (12.5, 25.0));
            Some(Point::new(1, 2))
        })
        .unwrap();
    assert_eq!(output.events.len(), 2);
    for event in output.events {
        let Event::Mouse(event) = event else { panic!() };
        assert_eq!(event.position, Point::new(1, 2));
    }
    let up = BrowserInput::decode(
        r#"{"type":"pointer","action":"up","x":-1,"y":0,"button":0,"buttons":0}"#,
        512,
    )
    .unwrap();
    assert!(up.normalize(32, |_, _| None).unwrap().cancel_pointer);
}

#[test]
fn failed_presentation_preserves_committed_output_and_retries_fully() {
    let mut surface = surface();
    surface
        .present(
            Damage::Full,
            |cx| {
                cx.fill(LocalRect::new(0, 0, 4, 2), "a", Style::default());
                hit(cx, "old", Rect::new(0, 0, 2, 1));
            },
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let previous = surface.presentation().hits().clone();
    assert!(
        surface
            .present(
                Damage::Full,
                |cx| hit(cx, "new", Rect::new(0, 0, 2, 1)),
                |_| Err("failed")
            )
            .is_err()
    );
    assert_eq!(surface.presentation().hits(), &previous);
    assert!(
        surface
            .route(pointer(MouseEventKind::Down(MouseButton::Left), 0, 0))
            .targets(&HitId::new("old"))
    );
    assert!(
        surface
            .present(
                Damage::None,
                |_| {},
                |frame| {
                    assert_eq!(frame.damage(), &Damage::Full);
                    Ok::<_, ()>(())
                }
            )
            .unwrap()
    );
}

#[test]
fn clipped_targets_focus_and_cancel_share_the_committed_scene() {
    let mut surface = surface();
    surface
        .present(
            Damage::Full,
            |cx| {
                hit(cx, "a", Rect::new(0, 0, 1, 1));
                hit(cx, "b", Rect::new(1, 0, 1, 1));
                hit(cx, "outside", Rect::new(8, 0, 1, 1));
            },
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(surface.presentation().hits().regions().len(), 2);
    surface.route(Event::Key(KeyStroke::simple(KeyCode::Tab)));
    let route = surface.route(Event::Paste("text".into()));
    assert!(route.targets(&HitId::new("b")));
    surface.route(pointer(MouseEventKind::Down(MouseButton::Left), 0, 0));
    let cancel = BrowserInput::decode(
        r#"{"type":"pointer","action":"cancel","x":0,"y":0,"button":0,"buttons":0}"#,
        512,
    )
    .unwrap()
    .normalize(32, |_, _| None)
    .unwrap();
    assert!(surface.route_input(cancel).is_empty());
    assert!(
        surface
            .route(pointer(MouseEventKind::Up(MouseButton::Left), 0, 0))
            .target
            .is_none()
    );
    assert!(
        surface
            .route(Event::Paste("text".into()))
            .targets(&HitId::new("a"))
    );
}

#[test]
fn nonfinite_and_extreme_coordinates_never_cast_or_overflow() {
    use bmux_web_renderer::input::BrowserModifiers;
    let wheel = |x, delta_x, delta_mode| BrowserInput::Wheel {
        x,
        y: 0.0,
        delta_x,
        delta_y: f64::MAX,
        delta_mode,
        modifiers: BrowserModifiers::default(),
    };
    assert_eq!(
        wheel(f64::NAN, 0.0, 0).normalize(32, |_, _| panic!("invalid point reached mapper")),
        Err(InputError::Invalid)
    );
    assert_eq!(
        wheel(0.0, f64::INFINITY, 0).normalize(32, |_, _| panic!("invalid delta reached mapper")),
        Err(InputError::Invalid)
    );
    assert_eq!(
        wheel(0.0, 0.0, 3).normalize(32, |_, _| panic!("invalid units reached mapper")),
        Err(InputError::Invalid)
    );
    assert!(
        wheel(f64::MAX, 1.0, 0)
            .normalize(32, |x, _| {
                assert_eq!(x, f64::MAX);
                None // Host rejects outside/overflowing cell coordinates before conversion.
            })
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(
        wheel(0.0, -f64::MAX, 2)
            .normalize(32, |_, _| Some(Point::new(u16::MAX, u16::MAX)))
            .unwrap()
            .events
            .len(),
        2
    );
}
