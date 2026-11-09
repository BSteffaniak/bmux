//! One presentation clock per frame, shared by layout, paint, and metadata.

use std::time::{Duration, Instant};

use bmux_tui::component::Component;
use bmux_tui::prelude::*;

const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
const PERIOD: Duration = Duration::from_millis(100);

/// A spinner whose glyph, width, and hit target all derive from the clock.
struct Spinner {
    start: Instant,
}

impl Spinner {
    fn step(&self, clock: Option<PresentationClock>) -> u64 {
        clock.map_or(0, |clock| clock.step(self.start, PERIOD))
    }
}

impl Component for Spinner {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        // Width grows with the step so layout visibly depends on the clock.
        let width = 1 + self.step(cx.clock()) % 3;
        LayoutNode::leaf(
            "spinner".into(),
            constraints.constrain(LogicalSize::new(width, 1)),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let step = self.step(cx.clock());
        let glyph = FRAMES[usize::try_from(step % 4).unwrap()];
        let width = u16::try_from(layout.size.width).unwrap();
        cx.write_line(
            LocalRect::new(0, 0, width, 1),
            &Line::raw(glyph.repeat(usize::from(width))),
        );
        cx.push_hit(HitRegion::new(
            format!("spinner-{step}"),
            Rect::new(0, 0, width, 1),
        ));
    }

    fn revision(&self) -> ComponentRevision {
        ComponentRevision::default()
    }
}

fn draw(terminal: &mut Terminal<Vec<u8>>, spinner: &Spinner) -> (String, Vec<String>, Vec<u16>) {
    terminal
        .draw(|cx| {
            let mut layout_cx = cx.layout_cx();
            let layout = spinner.layout(Constraints::loose(Size::new(8, 1)), &mut layout_cx);
            assert_eq!(layout_cx.clock(), cx.clock());
            spinner.paint(&layout, cx);
        })
        .unwrap();
    let row = terminal.retained_buffer().unwrap().row_symbols(0).unwrap();
    let hits = terminal.hits().regions();
    (
        row.trim_end().to_owned(),
        hits.iter().map(|hit| hit.id.as_str().to_owned()).collect(),
        hits.iter().map(|hit| hit.area.width).collect(),
    )
}

#[test]
fn layout_paint_and_hits_agree_on_the_supplied_frame_clock() {
    let start = Instant::now();
    let spinner = Spinner { start };
    let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 8, 1));
    for (elapsed, glyphs, id) in [
        (0, "|", "spinner-0"),
        (150, "//", "spinner-1"),
        (250, "---", "spinner-2"),
        (399, "\\", "spinner-3"),
    ] {
        let clock = PresentationClock::new(start + Duration::from_millis(elapsed));
        terminal.set_presentation_clock(Some(clock));
        assert_eq!(terminal.presentation_clock(), Some(clock));
        let (row, hits, widths) = draw(&mut terminal, &spinner);
        assert_eq!(row, glyphs);
        assert_eq!(hits, vec![id.to_owned()]);
        assert_eq!(widths, vec![u16::try_from(glyphs.chars().count()).unwrap()]);
    }
}

#[test]
fn clock_is_stable_until_the_driver_replaces_it() {
    let start = Instant::now();
    let spinner = Spinner { start };
    let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 8, 1));
    terminal.set_presentation_clock(Some(PresentationClock::new(start + PERIOD)));
    let first = draw(&mut terminal, &spinner);
    let second = draw(&mut terminal, &spinner);
    assert_eq!(first, second);
    assert_eq!(first.1, vec!["spinner-1".to_owned()]);
}

#[test]
fn frames_without_a_driver_clock_report_none() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
    let mut frame = Frame::new(&mut buffer);
    assert_eq!(frame.clock(), None);
    let cx = PaintCx::new(&mut frame);
    assert_eq!(cx.clock(), None);
    assert_eq!(cx.layout_cx().clock(), None);

    let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 8, 1));
    let (row, hits, _) = draw(
        &mut terminal,
        &Spinner {
            start: Instant::now(),
        },
    );
    assert_eq!(row, "|");
    assert_eq!(hits, vec!["spinner-0".to_owned()]);
}

#[test]
fn nested_contexts_and_forked_terminals_share_the_frame_clock() {
    let clock = PresentationClock::new(Instant::now());
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 4));
    let mut frame = Frame::new(&mut buffer);
    frame.set_clock(Some(clock));
    PaintCx::new(&mut frame).with_child(1, 1, LocalRect::new(0, 0, 2, 2), |cx| {
        assert_eq!(cx.clock(), Some(clock));
        cx.with_style(Style::new(), |cx| assert_eq!(cx.clock(), Some(clock)));
    });

    let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 4, 1));
    terminal.set_presentation_clock(Some(clock));
    let staged = terminal.fork(Vec::new());
    assert_eq!(staged.presentation_clock(), Some(clock));
    assert_eq!(LayoutCx::with_clock(clock).clock(), Some(clock));
    assert_eq!(LayoutCx::new().clock(), None);
}
