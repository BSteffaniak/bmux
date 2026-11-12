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

/// Minimal VT screen: CUP, SGR, DECSTBM, left/right margins (DECLRMM with
/// DECSLRM), SU/SD, cursor visibility and synchronized-output modes. Any
/// other control sequence fails the test, so the presenter cannot rely on
/// behaviour the model does not check.
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
    margins_mode: bool,
    left: u16,
    right: u16,
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
            margins_mode: false,
            left: 0,
            right: width,
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
            ("?69", 'h') => self.margins_mode = true,
            ("?69", 'l') => {
                self.margins_mode = false;
                (self.left, self.right) = (0, self.width);
            }
            ("", 's') if self.margins_mode => {
                (self.left, self.right) = (0, self.width);
                (self.x, self.y) = (0, 0);
            }
            (_, 's') if self.margins_mode => {
                let values = numbers(1);
                self.left = values[0] - 1;
                self.right = values[1];
                (self.x, self.y) = (0, 0);
            }
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
        let (left, right) = (usize::from(self.left), usize::from(self.right));
        let band: Vec<usize> = (usize::from(self.top)..usize::from(self.bottom)).collect();
        let order: Vec<usize> = if rows > 0 {
            band.clone()
        } else {
            band.iter().rev().copied().collect()
        };
        for y in order {
            let source = i64::try_from(y).unwrap() + rows;
            let source = usize::try_from(source)
                .ok()
                .filter(|source| band.contains(source));
            for x in left..right {
                self.cells[y * width + x] =
                    source.map_or_else(blank, |source| self.cells[source * width + x].clone());
            }
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

const SIDEBAR: u16 = 3;

/// A list row whose every cell differs from its neighbours' cells.
fn list_line(line: i64) -> String {
    let letter = char::from(b'a' + u8::try_from(line.rem_euclid(26)).unwrap());
    std::iter::repeat_n(letter, usize::from(LIST.width)).collect()
}
/// Body columns beside the sidebar.
const LIST: Rect = Rect::new(SIDEBAR, 0, WIDTH - SIDEBAR, HEIGHT - 1);

/// One frame with a sidebar: list lines starting at `first` beside sidebar
/// rows that repeat (`uniform`) or differ per row, reporting the list's shift
/// since `previous` as a paint-time scroll hint.
fn paint_with_sidebar(cx: &mut PaintCx<'_, '_>, first: i64, previous: i64, uniform: bool) {
    for row in 0..i64::from(LIST.height) {
        let side = if uniform {
            "|| ".to_owned()
        } else {
            format!("{row:02}|")
        };
        cx.write_line(LocalRect::new(0, row, SIDEBAR, 1), &Line::raw(side));
        cx.write_line(
            LocalRect::new(i32::from(SIDEBAR), row, LIST.width, 1),
            &Line::raw(list_line(first + row)),
        );
    }
    cx.write_line(
        LocalRect::new(0, i64::from(STATUS.y), WIDTH, 1),
        &Line::raw("status".to_owned()),
    );
    cx.hint_scroll(
        LocalRect::new(i32::from(SIDEBAR), 0, LIST.width, LIST.height),
        first - previous,
    );
}

/// Draw `before`, then `after` with a paint-time hint (full damage), replaying
/// both into a screen model; `expected` replays a full repaint of `after`.
fn replay_hinted(mode: ScrollRegionAcceleration, before: i64, after: i64, uniform: bool) -> Replay {
    let mut terminal = terminal(mode);
    let mut screen = Screen::new(WIDTH, HEIGHT);
    terminal
        .draw(|cx| paint_with_sidebar(cx, before, before, uniform))
        .unwrap();
    screen.feed(terminal.writer());
    terminal.writer_mut().clear();
    let stats = terminal
        .draw(|cx| paint_with_sidebar(cx, after, before, uniform))
        .unwrap();
    let output = terminal.writer().clone();
    screen.feed(&output);
    let mut reference = self::terminal(ScrollRegionAcceleration::Off);
    reference
        .draw(|cx| paint_with_sidebar(cx, after, after, uniform))
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

#[test]
fn painted_scroll_hints_match_a_full_repaint_in_every_mode() {
    let height = i64::from(LIST.height);
    for mode in [
        ScrollRegionAcceleration::Off,
        ScrollRegionAcceleration::Auto,
        ScrollRegionAcceleration::AutoWithMargins,
    ] {
        for uniform in [false, true] {
            for rows in (1 - height)..height {
                let replay = replay_hinted(mode, 100, 100 + rows, uniform);
                assert_eq!(
                    replay.screen.rows(),
                    replay.expected.rows(),
                    "{mode:?} shift {rows} (uniform sidebar {uniform})"
                );
                if mode == ScrollRegionAcceleration::Off || rows == 0 {
                    assert_eq!(replay.applied, None);
                }
            }
        }
    }
}

#[test]
fn a_narrow_hint_scrolls_full_rows_when_the_columns_beside_it_repeat() {
    let replay = replay_hinted(ScrollRegionAcceleration::Auto, 100, 101, true);
    let full = Rect::new(0, LIST.y, WIDTH, LIST.height);
    assert_eq!(
        replay.applied,
        Some(AppliedScroll {
            region: full,
            rows: 1
        })
    );
    assert!(
        replay.stats.changed_cells <= usize::from(WIDTH),
        "only the exposed row is rewritten, got {}",
        replay.stats.changed_cells
    );
}

#[test]
fn a_narrow_hint_beside_distinct_columns_scrolls_inside_margins_only_when_allowed() {
    let plain = replay_hinted(ScrollRegionAcceleration::Off, 100, 101, false);
    let auto = replay_hinted(ScrollRegionAcceleration::Auto, 100, 101, false);
    let full = Rect::new(0, LIST.y, WIDTH, LIST.height);
    assert_eq!(
        auto.applied,
        Some(AppliedScroll {
            region: full,
            rows: 1
        }),
        "rewriting the moved sidebar still costs less than rewriting the list"
    );
    assert!(!contains(&auto.output, "?69h"));
    let margins = replay_hinted(ScrollRegionAcceleration::AutoWithMargins, 100, 101, false);
    assert_eq!(
        margins.applied,
        Some(AppliedScroll {
            region: LIST,
            rows: 1
        })
    );
    assert!(contains(
        &margins.output,
        &format!(
            "\x1b[?69h\x1b[{};{}s\x1b[1;{}r\x1b[1S",
            SIDEBAR + 1,
            WIDTH,
            LIST.bottom()
        )
    ));
    assert!(margins.stats.changed_cells < auto.stats.changed_cells);
    assert!(auto.stats.changed_cells < plain.stats.changed_cells);
    assert_eq!(margins.screen.rows(), margins.expected.rows());
    assert_eq!(auto.screen.rows(), auto.expected.rows());
}

#[test]
fn wide_graphemes_across_a_margin_keep_rows_whole() {
    let mut terminal = terminal(ScrollRegionAcceleration::AutoWithMargins);
    let mut screen = Screen::new(WIDTH, HEIGHT);
    let frame = |cx: &mut PaintCx<'_, '_>, first: i64, previous: i64| {
        paint_with_sidebar(cx, first, previous, false);
        // A wide grapheme straddling the left margin on one row.
        cx.write_line(
            LocalRect::new(i32::from(SIDEBAR) - 1, 2, 2, 1),
            &Line::raw("界".to_owned()),
        );
    };
    terminal.draw(|cx| frame(cx, 100, 100)).unwrap();
    screen.feed(terminal.writer());
    terminal.writer_mut().clear();
    terminal.draw(|cx| frame(cx, 101, 100)).unwrap();
    screen.feed(terminal.writer());
    assert_ne!(
        terminal.applied_scroll().map(|scroll| scroll.region),
        Some(LIST),
        "a margin through a wide grapheme would split it"
    );
    let mut reference = self::terminal(ScrollRegionAcceleration::Off);
    reference.draw(|cx| frame(cx, 101, 101)).unwrap();
    let mut expected = Screen::new(WIDTH, HEIGHT);
    expected.feed(reference.writer());
    assert_eq!(screen.rows(), expected.rows());
}
