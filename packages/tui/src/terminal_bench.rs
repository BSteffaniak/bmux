//! What a frame costs [`Terminal`](crate::terminal::Terminal) at a large size, by phase.
//!
//! Run with `cargo test -p bmux_tui --release terminal_bench -- --ignored --nocapture`.
//! Each scenario paints the way a terminal multiplexer presents panes: every visible cell of a
//! pane each frame it changed, with the pane's scroll given as a hint, against a retained
//! frame. Correctness of the encoded output is covered by the terminal tests; this reports time
//! and bytes.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::damage::{Damage, DamagePolicy};
use crate::geometry::Rect;
use crate::paint::{LocalRect, PaintCx};
use crate::presentation::with_presentation_observer;
use crate::style::{Color, Modifier, Style};
use crate::terminal::{ScrollRegionAcceleration, Terminal};

const WIDTH: u16 = 420;
const HEIGHT: u16 = 90;

/// One line of a pane's content, generated from its line number: words with a style change every
/// few cells, like highlighted code.
fn line_cells(line: u64, width: u16) -> Vec<(char, Style)> {
    let mut cells = Vec::with_capacity(usize::from(width));
    let mut seed = line.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut style = Style::new();
    let indent = u16::try_from(line % 7).unwrap_or(0) * 2;
    for column in 0..width {
        if column % 6 == 0 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let fg = Color::Indexed(u8::try_from(seed % 200).unwrap_or(0) + 16);
            style = Style::new().fg(fg);
            if seed.is_multiple_of(5) {
                style = style.add_modifier(Modifier::BOLD);
            }
        }
        let symbol = if column < indent || (seed >> (column % 32)).is_multiple_of(9) {
            ' '
        } else {
            char::from(b'a' + u8::try_from((seed >> (column % 40)) % 26).unwrap_or(0))
        };
        cells.push((symbol, style));
    }
    cells
}

/// Lines of content, generated once so painting measures the terminal, not the generator.
const LINES: usize = 4096;

thread_local! {
    static CONTENT: Vec<Vec<(char, Style)>> =
        (0..LINES).map(|line| line_cells(line as u64, WIDTH)).collect();
}

/// A pane: a rect on screen showing lines `top..top + height` of its content.
struct Pane {
    rect: Rect,
    top: u64,
}

impl Pane {
    fn paint(&self, cx: &mut PaintCx<'_, '_>) {
        CONTENT.with(|content| {
            let mut symbol = [0_u8; 4];
            for row in 0..self.rect.height {
                let line = usize::try_from((self.top + u64::from(row)) % LINES as u64).unwrap_or(0);
                let cells = &content[line][..usize::from(self.rect.width)];
                let y = i64::from(self.rect.y + row);
                for (column, (ch, style)) in cells.iter().enumerate() {
                    let x = i32::from(self.rect.x) + i32::try_from(column).unwrap_or(i32::MAX);
                    cx.replace_cell(x, y, ch.encode_utf8(&mut symbol), *style);
                }
            }
        });
    }
}

#[derive(Default)]
struct Totals {
    frames: u32,
    draw: Duration,
    bytes: usize,
    phases: BTreeMap<String, u128>,
}

impl Totals {
    fn report(&self, name: &str) {
        let frames = u128::from(self.frames.max(1));
        let draw_us = self.draw.as_micros() / frames;
        let bytes = self.bytes / usize::try_from(self.frames.max(1)).unwrap_or(1);
        let phases = self
            .phases
            .iter()
            .map(|(phase, ns)| format!("{phase} {} us", ns / frames / 1000))
            .collect::<Vec<_>>()
            .join(", ");
        println!("{name}: {draw_us} us/frame, {bytes} B/frame | {phases}");
    }
}

/// Draw `frames` frames: before each, `step` moves the panes and returns the frame's damage.
fn run(
    name: &str,
    panes: &mut [Pane],
    frames: u32,
    mut step: impl FnMut(&mut [Pane], u32) -> (Damage, Vec<(usize, i64)>),
) {
    let area = Rect::new(0, 0, WIDTH, HEIGHT);
    let mut terminal = Terminal::new(Vec::new(), area);
    terminal.set_scroll_region_acceleration(ScrollRegionAcceleration::AutoWithMargins);
    terminal
        .draw(|cx| {
            for pane in panes.iter() {
                pane.paint(cx);
            }
        })
        .unwrap_or_else(|error| panic!("first frame: {error}"));
    let records = Rc::new(RefCell::new(Vec::new()));
    let mut totals = Totals::default();
    for frame in 0..frames {
        let (damage, scrolls) = step(panes, frame);
        // Paint what is damaged, as a multiplexer repaints only the panes that changed.
        let painted: Vec<bool> = panes
            .iter()
            .map(|pane| match &damage {
                Damage::Full => true,
                damage => damage
                    .retained_regions()
                    .iter()
                    .any(|region| !region.intersection(pane.rect).is_empty()),
            })
            .collect();
        terminal.writer_mut().clear();
        let sink = Rc::clone(&records);
        let started = Instant::now();
        with_presentation_observer(
            u64::from(frame),
            move |record| sink.borrow_mut().push(record),
            || {
                terminal
                    .draw_damage(damage, |cx| {
                        for (index, pane) in panes.iter().enumerate() {
                            if painted[index] {
                                pane.paint(cx);
                            }
                            for (scrolled, rows) in &scrolls {
                                if *scrolled == index {
                                    cx.hint_scroll(LocalRect::terminal(pane.rect), *rows);
                                }
                            }
                        }
                    })
                    .unwrap_or_else(|error| panic!("frame {frame}: {error}"));
            },
        );
        totals.draw += started.elapsed();
        totals.frames += 1;
        totals.bytes += terminal.writer().len();
        for record in records.borrow_mut().drain(..) {
            *totals
                .phases
                .entry(format!("{:?}", record.phase))
                .or_default() += record.elapsed_ns;
        }
    }
    totals.report(name);
}

fn policy() -> DamagePolicy {
    DamagePolicy::default()
}

#[test]
#[ignore = "benchmark; run with --release -- --ignored --nocapture"]
fn terminal_bench() {
    let area = Rect::new(0, 0, WIDTH, HEIGHT);
    let full = |panes: &[Pane]| -> Vec<Rect> { panes.iter().map(|pane| pane.rect).collect() };

    let mut one = [Pane { rect: area, top: 0 }];
    run("full screen, scroll 1 line", &mut one, 200, |panes, _| {
        panes[0].top += 1;
        (Damage::Full, vec![(0, 1)])
    });

    let mut one = [Pane { rect: area, top: 0 }];
    run(
        "full screen, scroll 1 line, regional damage",
        &mut one,
        200,
        |panes, _| {
            panes[0].top += 1;
            (
                Damage::regions(
                    full(panes),
                    area,
                    DamagePolicy {
                        max_area_percent: 101,
                        ..policy()
                    },
                ),
                vec![(0, 1)],
            )
        },
    );

    let mut three = [
        Pane {
            rect: Rect::new(0, 0, 140, HEIGHT),
            top: 0,
        },
        Pane {
            rect: Rect::new(140, 0, 140, HEIGHT),
            top: 1000,
        },
        Pane {
            rect: Rect::new(280, 0, 140, HEIGHT),
            top: 2000,
        },
    ];
    run("three panes, one scrolls", &mut three, 200, |panes, _| {
        panes[1].top += 1;
        (
            Damage::regions([panes[1].rect], area, policy()),
            vec![(1, 1)],
        )
    });

    let mut three = [
        Pane {
            rect: Rect::new(0, 0, 140, HEIGHT),
            top: 0,
        },
        Pane {
            rect: Rect::new(140, 0, 140, HEIGHT),
            top: 1000,
        },
        Pane {
            rect: Rect::new(280, 0, 140, HEIGHT),
            top: 2000,
        },
    ];
    run(
        "three panes, two scroll by different amounts",
        &mut three,
        200,
        |panes, _| {
            panes[0].top += 1;
            panes[2].top += 2;
            (
                Damage::regions(
                    [panes[0].rect, panes[2].rect],
                    area,
                    DamagePolicy {
                        max_area_percent: 101,
                        ..policy()
                    },
                ),
                vec![(0, 1), (2, 2)],
            )
        },
    );

    let mut row = [Pane {
        rect: Rect::new(0, 0, WIDTH, 1),
        top: 0,
    }];
    run(
        "full screen, one row changes",
        &mut row,
        200,
        |panes, frame| {
            // One row is damaged and repainted with new content, as a status line or an edited
            // line of an otherwise still pane costs.
            let y = u16::try_from(frame % u32::from(HEIGHT)).unwrap_or(0);
            panes[0].rect = Rect::new(0, y, WIDTH, 1);
            panes[0].top = u64::from(frame) + 5000;
            (Damage::regions([panes[0].rect], area, policy()), Vec::new())
        },
    );
}
