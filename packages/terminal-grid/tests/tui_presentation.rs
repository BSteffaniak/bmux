//! What bmux_tui's `Terminal` writes, replayed into a terminal grid, shows exactly the frame it
//! drew: across regional damage, scroll hints over panes, wide graphemes and colors.
//!
//! Scrolls inside left/right margins (`ScrollRegionAcceleration::AutoWithMargins`) are not
//! replayed here: the grid does not implement left/right margin mode (DEC private mode 69).

use bmux_terminal_grid::{Color as GridColor, GridLimits, TerminalGridStream};
use bmux_tui::buffer::Buffer;
use bmux_tui::damage::{Damage, DamagePolicy};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::terminal::{ScrollRegionAcceleration, Terminal};

const WIDTH: u16 = 40;
const HEIGHT: u16 = 12;

/// Line `line` of a pane's content: words, a wide grapheme now and then, and colors.
fn line_cells(line: u64, width: u16) -> Vec<(&'static str, Style)> {
    let mut seed = line.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let words = ["ab", "cde", " ", "\u{754c}", "f", "  ", "gh\u{754c}"];
    let mut cells = Vec::new();
    let mut used = 0;
    while used < width {
        let word = words[usize::try_from(next() % 7).unwrap()];
        let mut style = match next() % 4 {
            0 => Style::new(),
            1 => Style::new().fg(Color::Indexed(100)),
            2 => Style::new().bg(Color::Indexed(200)),
            _ => Style::new().fg(Color::Indexed(30)).bg(Color::Indexed(60)),
        };
        for (bit, modifier) in [
            Modifier::BOLD,
            Modifier::DIM,
            Modifier::ITALIC,
            Modifier::UNDERLINE,
            Modifier::REVERSED,
            Modifier::CROSSED_OUT,
        ]
        .into_iter()
        .enumerate()
        {
            if next() >> (bit * 3) & 3 == 0 {
                style = style.add_modifier(modifier);
            }
        }
        let mut chars = word.char_indices().peekable();
        while let Some((start, ch)) = chars.next() {
            let end = chars.peek().map_or(word.len(), |(end, _)| *end);
            let symbol = &word[start..end];
            let cell_width = if ch == '\u{754c}' { 2 } else { 1 };
            if used + cell_width > width {
                used = width;
                break;
            }
            cells.push((symbol, style));
            used += cell_width;
        }
    }
    cells
}

struct Pane {
    rect: Rect,
    top: u64,
}

impl Pane {
    fn paint(&self, cx: &mut PaintCx<'_, '_>) {
        cx.fill(LocalRect::terminal(self.rect), " ", Style::new());
        self.paint_rows(cx, 0..self.rect.height);
    }

    /// Repaint rows `rows` (pane-relative) over whatever they show.
    fn paint_rows(&self, cx: &mut PaintCx<'_, '_>, rows: std::ops::Range<u16>) {
        for row in rows.clone() {
            cx.fill(
                LocalRect::terminal(Rect::new(
                    self.rect.x,
                    self.rect.y + row,
                    self.rect.width,
                    1,
                )),
                " ",
                Style::new(),
            );
        }
        for row in rows {
            let mut x = i32::from(self.rect.x);
            for (symbol, style) in line_cells(self.top + u64::from(row), self.rect.width) {
                cx.replace_cell(x, i64::from(self.rect.y + row), symbol, style);
                x += if symbol == "\u{754c}" { 2 } else { 1 };
            }
        }
    }
}

fn grid_color(color: Option<Color>) -> Option<GridColor> {
    match color {
        None | Some(Color::Default) => None,
        Some(Color::Indexed(index)) => Some(GridColor::Indexed(index)),
        Some(other) => panic!("unexpected color {other:?}"),
    }
}

fn assert_host_shows(host: &TerminalGridStream, expected: &Buffer, context: &str) {
    let grid = host.grid();
    for y in 0..HEIGHT {
        let row = grid.viewport_row_ref(usize::from(y)).expect("host row");
        for x in 0..WIDTH {
            let cell = expected.get(Point::new(x, y)).expect("expected cell");
            if cell.is_wide_continuation() {
                continue;
            }
            let shown = row.cells().get(usize::from(x));
            let text = shown.map_or(" ", |cell| {
                if cell.text().is_empty() {
                    " "
                } else {
                    cell.text()
                }
            });
            let style = shown
                .map(|cell| grid.palette().get(cell.style()))
                .unwrap_or_default();
            let modifiers = cell.style.modifiers;
            assert_eq!(
                (
                    text,
                    style.fg,
                    style.bg,
                    [style.bold, style.dim, style.italic, style.underline],
                    [style.inverse, style.strike]
                ),
                (
                    cell.symbol.as_str(),
                    grid_color(cell.style.fg),
                    grid_color(cell.style.bg),
                    [
                        modifiers.contains(Modifier::BOLD),
                        modifiers.contains(Modifier::DIM),
                        modifiers.contains(Modifier::ITALIC),
                        modifiers.contains(Modifier::UNDERLINE)
                    ],
                    [
                        modifiers.contains(Modifier::REVERSED),
                        modifiers.contains(Modifier::CROSSED_OUT)
                    ]
                ),
                "{context}: host differs at column {x} row {y}"
            );
        }
    }
}

fn run(acceleration: ScrollRegionAcceleration, over: bool) {
    let area = Rect::new(0, 0, WIDTH, HEIGHT);
    let mut panes = [
        Pane {
            rect: Rect::new(0, 0, 14, HEIGHT),
            top: 0,
        },
        Pane {
            rect: Rect::new(14, 0, 13, 7),
            top: 500,
        },
        Pane {
            rect: Rect::new(14, 7, 13, 5),
            top: 900,
        },
        Pane {
            rect: Rect::new(27, 0, 13, HEIGHT),
            top: 1300,
        },
    ];
    let mut terminal = Terminal::new(Vec::new(), area);
    terminal.set_scroll_region_acceleration(acceleration);
    let mut host = TerminalGridStream::new(WIDTH, HEIGHT, GridLimits::default()).expect("host");
    let mut seed = 0x1234_5678_9abc_def1_u64;
    let mut next = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    for frame in 0..1500 {
        // Each frame, some panes scroll (by up to a few rows either way) and are repainted.
        let mut changed = Vec::new();
        let mut scrolls = Vec::new();
        for (index, pane) in panes.iter_mut().enumerate() {
            if frame == 0 || next(3) == 0 {
                let was = pane.top;
                pane.top = pane
                    .top
                    .saturating_add_signed(i64::try_from(next(7)).unwrap() - 3);
                let rows = i64::try_from(pane.top).unwrap() - i64::try_from(was).unwrap();
                changed.push(index);
                if rows != 0 {
                    scrolls.push((index, rows));
                }
            }
        }
        let damage = if frame == 0 || next(20) == 0 {
            Damage::Full
        } else {
            Damage::regions(
                changed.iter().map(|index| panes[*index].rect),
                area,
                DamagePolicy::default(),
            )
        };
        let full = matches!(damage, Damage::Full);
        terminal.writer_mut().clear();
        let paint = |cx: &mut PaintCx<'_, '_>| {
            for (index, pane) in panes.iter().enumerate() {
                if full || changed.contains(&index) {
                    pane.paint(cx);
                }
            }
            for (index, rows) in &scrolls {
                cx.hint_scroll(LocalRect::terminal(panes[*index].rect), *rows);
            }
        };
        let extra = u16::try_from(next(u64::from(HEIGHT))).unwrap();
        if over && !full {
            // Scrolling panes move their cells and repaint only the rows the move exposed (and
            // one more); other changed panes repaint in full.
            terminal
                .draw_over(|cx| {
                    for (index, pane) in panes.iter().enumerate() {
                        if !changed.contains(&index) {
                            continue;
                        }
                        let Some((_, rows)) =
                            scrolls.iter().find(|(scrolled, _)| *scrolled == index)
                        else {
                            pane.paint(cx);
                            continue;
                        };
                        // Now and then a row is repainted as it was before the move carries it
                        // along, which leaves the painted cells where nothing was painted.
                        if extra % 3 == 0 {
                            let before = Pane {
                                rect: pane.rect,
                                top: pane.top.saturating_add_signed(-*rows),
                            };
                            let row = extra % pane.rect.height;
                            before.paint_rows(cx, row..row + 1);
                        }
                        cx.move_cells(LocalRect::terminal(pane.rect), *rows);
                        let height = pane.rect.height;
                        let moved = u16::try_from(rows.unsigned_abs()).unwrap().min(height);
                        let exposed = if *rows > 0 {
                            height - moved..height
                        } else {
                            0..moved
                        };
                        pane.paint_rows(cx, exposed);
                        let extra = extra % height;
                        pane.paint_rows(cx, extra..extra + 1);
                    }
                })
                .expect("draw over");
        } else {
            terminal.draw_damage(damage, paint).expect("draw");
        }
        host.process(terminal.writer());
        let mut expected = Buffer::empty(area);
        {
            let mut frame = Frame::new(&mut expected);
            let mut cx = PaintCx::new(&mut frame);
            for pane in &panes {
                pane.paint(&mut cx);
            }
        }
        assert_host_shows(&host, &expected, &format!("{acceleration:?} frame {frame}"));
    }
}

#[test]
fn the_host_shows_every_frame_without_scroll_regions() {
    run(ScrollRegionAcceleration::Off, false);
}

#[test]
fn the_host_shows_every_frame_with_full_row_scrolls() {
    run(ScrollRegionAcceleration::Auto, false);
}

#[test]
fn the_host_shows_every_frame_painted_over_the_last() {
    run(ScrollRegionAcceleration::Auto, true);
}

#[test]
fn streamed_cursor_and_tui_wrap_agree_at_right_margin() {
    use bmux_tui::text_width::{display_width, wrap_text_with_continuation_character};
    for (text, rows, col) in [
        ("abce\u{301}X", ["abce\u{301}", "X"], 1),
        ("abc界X", ["abc", "界X"], 3),
        ("abc👩‍💻X", ["abc", "👩‍💻X"], 3),
        ("abلاX", ["abلا", "X"], 1),
    ] {
        assert_eq!(wrap_text_with_continuation_character(text, 4, 4), rows);
        assert_eq!(display_width(rows[1]), col);
        let mut host = TerminalGridStream::new(4, 3, GridLimits::default()).unwrap();
        for byte in text.bytes() {
            host.process(&[byte]);
        }
        let grid = host.grid();
        assert_eq!((grid.cursor().row, grid.cursor().col), (1, col), "{text}");
        for (index, expected) in rows.iter().enumerate() {
            let row = grid.viewport_row_ref(index).unwrap();
            let actual: String = row.cells().iter().map(|cell| cell.text()).collect();
            assert_eq!(actual.trim_end(), *expected);
        }
    }
}
