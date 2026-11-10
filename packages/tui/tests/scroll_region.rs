//! Scroll damage presented with terminal scroll regions.
//!
//! Every accelerated frame is replayed into a small terminal screen model and
//! compared with the same model after a complete repaint of the expected
//! frame, so acceleration can only change the bytes written, never what the
//! terminal shows.

use bmux_tui::image::{ImageFit, ImageLifecycle};
use bmux_tui::pointer::PointerShapeCapability;
use bmux_tui::prelude::*;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const WIDTH: u16 = 12;
const HEIGHT: u16 = 8;
/// Body rows that scroll; the last row is a status line outside the region.
const BODY: Rect = Rect::new(0, 0, WIDTH, HEIGHT - 1);
const STATUS: Rect = Rect::new(0, HEIGHT - 1, WIDTH, 1);

/// Minimal VT screen: CUP, SGR, DECSTBM, SU/SD, cursor visibility and
/// synchronized-output modes. Any other control sequence fails the test, so
/// the presenter cannot rely on behaviour the model does not check.
struct Screen {
    width: u16,
    height: u16,
    /// (symbol, SGR parameters); a wide continuation stores an empty symbol.
    cells: Vec<(String, String)>,
    x: u16,
    y: u16,
    sgr: String,
    top: u16,
    bottom: u16,
}

impl Screen {
    fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            cells: vec![blank(); usize::from(width) * usize::from(height)],
            x: 0,
            y: 0,
            sgr: "0".to_owned(),
            top: 0,
            bottom: height,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let text = std::str::from_utf8(bytes).expect("utf-8 output");
        let mut rest = text;
        while !rest.is_empty() {
            if let Some(sequence) = rest.strip_prefix("\x1b[") {
                let end = sequence
                    .find(|c: char| ('\x40'..='\x7e').contains(&c))
                    .expect("terminated CSI");
                self.csi(&sequence[..end], sequence.as_bytes()[end] as char);
                rest = &sequence[end + 1..];
            } else if let Some(sequence) = rest.strip_prefix("\x1b]") {
                let end = sequence.find('\x07').expect("terminated OSC");
                rest = &sequence[end + 1..];
            } else {
                let end = rest.find('\x1b').unwrap_or(rest.len());
                for grapheme in rest[..end].graphemes(true) {
                    self.print(grapheme);
                }
                rest = &rest[end..];
            }
        }
    }

    fn csi(&mut self, params: &str, command: char) {
        let numbers = |default: u16| -> Vec<u16> {
            params
                .split(';')
                .map(|value| value.parse().unwrap_or(default))
                .collect()
        };
        match (params, command) {
            ("?25" | "?2026", 'h' | 'l') => {}
            (_, 'm') => self.sgr = if params.is_empty() { "0" } else { params }.to_owned(),
            (_, 'H') => {
                let values = numbers(1);
                self.y = values[0] - 1;
                self.x = values.get(1).copied().unwrap_or(1) - 1;
            }
            ("", 'r') => {
                self.top = 0;
                self.bottom = self.height;
                (self.x, self.y) = (0, 0);
            }
            (_, 'r') => {
                let values = numbers(1);
                self.top = values[0] - 1;
                self.bottom = values[1];
                (self.x, self.y) = (0, 0);
            }
            (_, 'S') => self.scroll(i64::from(numbers(1)[0])),
            (_, 'T') => self.scroll(-i64::from(numbers(1)[0])),
            _ => panic!("unmodelled control sequence ESC[{params}{command}"),
        }
    }

    fn scroll(&mut self, rows: i64) {
        let width = usize::from(self.width);
        let band = &mut self.cells[usize::from(self.top) * width..usize::from(self.bottom) * width];
        let moved = (usize::try_from(rows.unsigned_abs()).unwrap() * width).min(band.len());
        if rows > 0 {
            band.rotate_left(moved);
            let start = band.len() - moved;
            band[start..].fill(blank());
        } else {
            band.rotate_right(moved);
            band[..moved].fill(blank());
        }
    }

    fn print(&mut self, grapheme: &str) {
        let width = u16::try_from(grapheme.width().max(1)).unwrap();
        let index = usize::from(self.y) * usize::from(self.width) + usize::from(self.x);
        self.cells[index] = (grapheme.to_owned(), self.sgr.clone());
        if width == 2 {
            self.cells[index + 1] = (String::new(), self.sgr.clone());
        }
        self.x += width;
    }

    fn rows(&self) -> Vec<String> {
        self.cells
            .chunks(usize::from(self.width))
            .map(|row| row.iter().map(|(symbol, _)| symbol.as_str()).collect())
            .collect()
    }
}

fn blank() -> (String, String) {
    (" ".to_owned(), "0".to_owned())
}

fn terminal(mode: ScrollRegionAcceleration) -> Terminal<Vec<u8>> {
    let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, WIDTH, HEIGHT));
    terminal.set_pointer_capability(PointerShapeCapability::Unsupported);
    terminal.set_scroll_region_acceleration(mode);
    terminal
}

/// One frame: body lines starting at `first`, then a status line.
fn paint(cx: &mut PaintCx<'_, '_>, first: i64, status: &str, styled: bool) {
    for row in 0..i64::from(BODY.height) {
        let line = first + row;
        let text = if line % 3 == 0 {
            format!("界{line:03}x")
        } else {
            format!("line {line:03}")
        };
        let style = if styled && line % 2 == 0 {
            Style::new().fg(Color::Rgb(200, 10, 10))
        } else {
            Style::new()
        };
        cx.write_line(
            LocalRect::new(0, row, WIDTH, 1),
            &Line::from_spans(vec![Span::styled(text, style)]),
        );
    }
    cx.write_line(
        LocalRect::new(0, i64::from(STATUS.y), WIDTH, 1),
        &Line::raw(status.to_owned()),
    );
}

struct Replay {
    screen: Screen,
    expected: Screen,
    output: Vec<u8>,
    stats: DrawStats,
    applied: Option<AppliedScroll>,
}

/// Draw frame `before`, then frame `after` with `damage`, replaying the
/// output into a screen model; `expected` replays a full repaint of `after`.
fn replay(
    mode: ScrollRegionAcceleration,
    before: i64,
    after: i64,
    damage: Damage,
    styled: bool,
) -> Replay {
    let mut terminal = terminal(mode);
    let mut screen = Screen::new(WIDTH, HEIGHT);
    terminal
        .draw(|cx| paint(cx, before, "status A", styled))
        .unwrap();
    screen.feed(terminal.writer());
    terminal.writer_mut().clear();
    let stats = terminal
        .draw_damage(damage, |cx| paint(cx, after, "status B", styled))
        .unwrap();
    let output = terminal.writer().clone();
    screen.feed(&output);

    let mut reference = self::terminal(ScrollRegionAcceleration::Off);
    reference
        .draw(|cx| paint(cx, after, "status B", styled))
        .unwrap();
    let mut expected = Screen::new(WIDTH, HEIGHT);
    expected.feed(reference.writer());
    Replay {
        screen,
        expected,
        output,
        stats,
        applied: terminal.applied_scroll(),
    }
}

fn scroll_damage(rows: i64) -> Damage {
    Damage::scroll(
        BODY,
        rows,
        [STATUS],
        Rect::new(0, 0, WIDTH, HEIGHT),
        DamagePolicy {
            max_regions: 64,
            max_area_percent: 100,
        },
    )
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[test]
fn accelerated_scroll_matches_a_full_repaint_for_every_shift() {
    let height = i64::from(BODY.height);
    for styled in [false, true] {
        for rows in (1 - height)..height {
            let replay = replay(
                ScrollRegionAcceleration::Auto,
                10,
                10 + rows,
                scroll_damage(rows),
                styled,
            );
            assert_eq!(
                replay.screen.cells,
                replay.expected.cells,
                "shift {rows} (styled {styled}) left the screen at {:?}, expected {:?}",
                replay.screen.rows(),
                replay.expected.rows()
            );
            assert!(!replay.stats.full_repaint);
            if rows == 0 {
                assert_eq!(replay.applied, None);
                assert!(!contains(&replay.output, "r\x1b["));
            } else {
                assert_eq!(replay.applied, Some(AppliedScroll { region: BODY, rows }));
                let command = if rows > 0 { 'S' } else { 'T' };
                assert!(contains(
                    &replay.output,
                    &format!("\x1b[1;{}r\x1b[{}{command}", BODY.bottom(), rows.abs())
                ));
            }
        }
    }
}

#[test]
fn accelerated_scroll_writes_only_exposed_and_changed_rows() {
    let accelerated = replay(
        ScrollRegionAcceleration::Auto,
        10,
        11,
        scroll_damage(1),
        false,
    );
    let repainted = replay(
        ScrollRegionAcceleration::Off,
        10,
        11,
        scroll_damage(1),
        false,
    );
    assert_eq!(accelerated.screen.cells, accelerated.expected.cells);
    assert_eq!(repainted.screen.cells, repainted.expected.cells);
    assert_eq!(repainted.applied, None);
    assert!(
        accelerated.stats.changed_cells <= usize::from(WIDTH) * 2,
        "one exposed row plus the status row, got {}",
        accelerated.stats.changed_cells
    );
    assert!(accelerated.stats.changed_cells < repainted.stats.changed_cells);
}

#[test]
fn unsafe_scroll_damage_falls_back_to_the_cell_diff() {
    let viewport = Rect::new(0, 0, WIDTH, HEIGHT);
    let narrow = Rect::new(0, 0, WIDTH - 1, BODY.height);
    let cases = [
        (
            "region narrower than the presentation",
            Damage::Scroll {
                region: narrow,
                rows: 1,
                regions: vec![Rect::new(0, 0, WIDTH, HEIGHT - 1), STATUS],
            },
        ),
        (
            "shift as tall as the region",
            Damage::Scroll {
                region: BODY,
                rows: i64::from(BODY.height),
                regions: vec![BODY, STATUS],
            },
        ),
        (
            "plain region damage",
            Damage::regions([BODY, STATUS], viewport, DamagePolicy::default()),
        ),
    ];
    for (name, damage) in cases {
        let replay = replay(ScrollRegionAcceleration::Auto, 10, 11, damage, false);
        assert_eq!(replay.applied, None, "{name}");
        assert!(!contains(&replay.output, "r\x1b["), "{name}");
        assert_eq!(replay.screen.cells, replay.expected.cells, "{name}");
    }
}

#[test]
fn image_placements_disable_scroll_regions() {
    let mut terminal = terminal(ScrollRegionAcceleration::Auto);
    let image = |cx: &mut PaintCx<'_, '_>| {
        cx.push_image(ImageContribution::Present(ImagePlacement {
            key: ImageKey::new("picture"),
            payload: ImagePayload::Png {
                bytes: Vec::new(),
                width: 1,
                height: 1,
            },
            destination: Rect::new(0, 0, 2, 1),
            fit: ImageFit::Contain,
            allocation: None,
            clip: Rect::new(0, 0, 2, 1),
            lifecycle: ImageLifecycle::Frame,
        }));
    };
    terminal
        .draw(|cx| {
            paint(cx, 10, "status A", false);
            image(cx);
        })
        .unwrap();
    terminal.writer_mut().clear();
    terminal
        .draw_damage(scroll_damage(1), |cx| {
            paint(cx, 11, "status B", false);
            image(cx);
        })
        .unwrap();
    assert_eq!(terminal.applied_scroll(), None);
    assert!(!contains(terminal.writer(), "r\x1b["));
}

#[test]
fn first_frame_and_resize_promote_scroll_damage_to_full() {
    let mut terminal = terminal(ScrollRegionAcceleration::Auto);
    let stats = terminal
        .draw_damage(scroll_damage(1), |cx| paint(cx, 10, "status A", false))
        .unwrap();
    assert!(stats.full_repaint);
    assert!(terminal.committed_damage().is_full());
    assert_eq!(terminal.applied_scroll(), None);

    terminal
        .draw_damage(scroll_damage(1), |cx| paint(cx, 11, "status A", false))
        .unwrap();
    assert_eq!(terminal.committed_damage().scroll_shift(), Some((BODY, 1)));
    assert!(terminal.applied_scroll().is_some());

    terminal.resize(Rect::new(0, 0, WIDTH, HEIGHT));
    terminal.reset();
    terminal
        .draw_damage(scroll_damage(1), |cx| paint(cx, 12, "status A", false))
        .unwrap();
    assert!(terminal.committed_damage().is_full());
    assert_eq!(terminal.applied_scroll(), None);

    terminal
        .draw_damage(Damage::None, |_| panic!("nothing to paint"))
        .unwrap();
    assert!(terminal.committed_damage().is_none());
}

#[test]
fn scroll_damage_retains_cells_and_metadata_outside_its_regions() {
    let mut terminal = terminal(ScrollRegionAcceleration::Auto);
    terminal
        .draw(|cx| {
            paint(cx, 10, "status A", false);
            cx.push_hit(HitRegion::new("status", STATUS));
        })
        .unwrap();
    // Only the body scrolls; the untouched status row keeps its cells and hit.
    let damage = Damage::scroll(
        BODY,
        1,
        [],
        Rect::new(0, 0, WIDTH, HEIGHT),
        DamagePolicy::default(),
    );
    terminal
        .draw_damage(damage, |cx| paint(cx, 11, "ignored", false))
        .unwrap();
    let buffer = terminal.retained_buffer().unwrap();
    assert_eq!(buffer.row_symbols(STATUS.y).unwrap().trim_end(), "status A");
    assert_eq!(buffer.row_symbols(0).unwrap().trim_end(), "line 011");
    assert!(
        terminal
            .hits()
            .regions()
            .iter()
            .any(|region| region.id.as_str() == "status")
    );
}
