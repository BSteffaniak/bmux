use bmux_tui::component::Component;
use bmux_tui::prelude::*;

#[test]
fn tall_surface_keeps_background_border_and_child_at_deep_scroll() {
    struct Tall;
    impl Component for Tall {
        fn layout(&self, constraints: Constraints, _: &mut LayoutCx) -> LayoutNode {
            LayoutNode::leaf(
                "tall".into(),
                constraints.constrain(LogicalSize::new(6, 100_000)),
            )
        }
        fn paint(&self, _: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
            cx.set_cell(0, 90_000, "x", Style::new());
        }
        fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
            if let Event::Mouse(mouse) = event
                && cx
                    .find_visible_rect(&layout.id)
                    .is_some_and(|rect| rect.contains(mouse.position))
            {
                EventOutcome::Handled
            } else {
                EventOutcome::Ignored
            }
        }
        fn revision(&self) -> ComponentRevision {
            ComponentRevision::default()
        }
    }
    let surface = Surface::new(Tall)
        .background(Style::new().bg(Color::Blue))
        .border(Border::single());
    let surface = Column::new()
        .child(Row::new().child(Keyed::new("wrapped-surface", Stack::new().child(surface))));
    let layout = surface.layout(Constraints::for_width(8), &mut LayoutCx::new());
    let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
    let mut frame = Frame::new(&mut buffer);
    PaintCx::new(&mut frame).with_child(0, -90_000, LocalRect::new(0, 90_000, 8, 3), |cx| {
        surface.paint(&layout, cx)
    });
    assert_eq!(
        frame.buffer().get(Point::new(4, 2)).unwrap().style.bg,
        Some(Color::Blue)
    );
    let viewport = Rect::new(0, 0, 8, 3);
    let mut events = EventCx::with_clip(&layout, viewport);
    events.with_transform(0, 0, 0, -90_000, viewport, |cx| {
        for (point, expected) in [
            (Point::new(1, 1), EventOutcome::Handled),
            (Point::new(0, 1), EventOutcome::Ignored),
            (Point::new(1, 3), EventOutcome::Ignored),
        ] {
            let event = Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                point,
            ));
            assert_eq!(surface.event(&event, &layout, cx), expected);
        }
    });
    assert_eq!(frame.buffer().get(Point::new(1, 1)).unwrap().symbol, "x");
    assert_ne!(frame.buffer().get(Point::new(0, 1)).unwrap().symbol, " ");
    assert_ne!(frame.buffer().get(Point::new(7, 1)).unwrap().symbol, " ");
}

#[test]
fn text_block_paints_rows_beyond_terminal_height() {
    let text = TextBlock::new(Text::from_lines(
        (0..70_003)
            .map(|row| Line::raw(format!("{row:05}")))
            .collect::<Vec<_>>(),
    ));
    let layout = text.layout(Constraints::for_width(5), &mut LayoutCx::new());
    assert_eq!(layout.size.height, 70_003);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 3));
    let mut frame = Frame::new(&mut buffer);
    PaintCx::new(&mut frame).with_child(0, -70_000, LocalRect::new(0, 70_000, 5, 3), |cx| {
        text.paint(&layout, cx)
    });
    for row in 0..3 {
        assert_eq!(
            frame.buffer().row_symbols(row),
            Some(format!("{}", 70_000 + usize::from(row)))
        );
    }
}

#[test]
fn message_card_sketch_uses_only_canonical_composition() {
    let card = Surface::new(
        Column::new()
            .gap(1)
            .child(
                Row::new()
                    .gap(1)
                    .child(TextBlock::new("Ada").style(Style::new().add_modifier(Modifier::BOLD)))
                    .flex(Flex::new(
                        1,
                        TextBlock::new("10:42").alignment(Alignment::Right),
                    )),
            )
            .child(TextBlock::new(
                "A variable-height message wraps without precomputing its child height.",
            )),
    )
    .id("message:42")
    .background(Style::new().bg(Color::Blue))
    .padding(Insets::new(1, 1, 1, 1));

    let layout = card.layout(Constraints::for_width(24), &mut LayoutCx::new());
    assert_eq!(layout.size.height, 8);

    let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 8));
    let mut frame = Frame::new(&mut buffer);
    card.paint(&layout, &mut PaintCx::new(&mut frame));

    assert_eq!(
        frame.buffer().row_symbols(0).as_deref(),
        Some("                        ")
    );
    assert_eq!(
        frame.buffer().row_symbols(1).as_deref(),
        Some(" Ada              10:42 ")
    );
    assert_eq!(
        frame.buffer().row_symbols(3).as_deref(),
        Some(" A variable-height      ")
    );
    assert_eq!(
        frame
            .buffer()
            .get(Point::new(23, 7))
            .and_then(|cell| cell.style.bg),
        Some(Color::Blue)
    );
}
