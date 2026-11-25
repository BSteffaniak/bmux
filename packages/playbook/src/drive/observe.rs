//! What an agent is shown of a terminal: its rows as text, where the cursor
//! is, which terminal modes the program has turned on, and which cells are
//! styled differently from the rest (selection bars, focus, errors).
//!
//! Rows and columns are 1-based everywhere an agent reads or types them.

use std::collections::HashMap;
use std::fmt::Write as _;

use bmux_terminal_grid::{Color, GridSnapshot, Style};
use serde::Serialize;

/// The screen at one moment.
#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    pub cols: u16,
    pub rows: u16,
    /// One entry per screen row, trailing blanks trimmed.
    pub lines: Vec<String>,
    /// 1-based cursor row and column.
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    /// The program is on the alternate screen (full-screen TUI mode).
    pub alternate_screen: bool,
    pub modes: Modes,
    /// The style most cells have; spans report differences from it.
    pub base_style: String,
    /// Runs of cells styled differently from `base_style`, in reading order.
    pub spans: Vec<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 1-based rows whose text changed since the previous observation.
    pub changed_rows: Vec<u16>,
    /// 1-based rows whose text is the same but whose styling changed (a
    /// selection or focus moving).
    pub restyled_rows: Vec<u16>,
    /// A signature of each row's styling, for the next comparison.
    #[serde(skip)]
    pub row_styles: Vec<u64>,
}

/// Terminal input modes the program has enabled.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // Independent terminal modes.
pub struct Modes {
    /// Mouse reporting: off, press, press-release, drag or motion.
    pub mouse: String,
    /// How mouse events are encoded: sgr, utf8 or x10.
    pub mouse_encoding: String,
    pub bracketed_paste: bool,
    pub application_cursor_keys: bool,
    pub application_keypad: bool,
}

/// A run of cells on one row sharing a style that stands out.
#[derive(Debug, Clone, Serialize)]
pub struct Span {
    pub row: u16,
    /// Last row of a span repeated on consecutive rows (a border, a column).
    pub last_row: u16,
    pub col: u16,
    pub width: u16,
    pub text: String,
    pub style: String,
    /// Paints a background or inverts: a selection, focus or status bar,
    /// rather than colored text.
    pub highlight: bool,
}

impl Observation {
    /// Build from the pane runtime's screen snapshot (`encoded_grid`).
    pub fn from_snapshot(
        snapshot: &GridSnapshot,
        previous: &[String],
        previous_styles: &[u64],
    ) -> Self {
        let width = usize::from(snapshot.width);
        let mut cells: Vec<Vec<(String, u32)>> = Vec::with_capacity(snapshot.rows.len());
        for row in &snapshot.rows {
            let mut line: Vec<(String, u32)> = vec![(" ".to_owned(), 0); width];
            for run in &row.runs {
                let mut col = usize::from(run.start_col);
                for grapheme in bmux_unicode_segmentation::graphemes(&run.text) {
                    if col >= width {
                        break;
                    }
                    let columns = bmux_cell_geometry::grapheme_width(grapheme).max(1);
                    line[col] = (grapheme.to_owned(), run.style.0);
                    for spacer in 1..columns {
                        if let Some(cell) = line.get_mut(col + spacer) {
                            // Spacer cells of a wide glyph carry its style but no text.
                            *cell = (String::new(), run.style.0);
                        }
                    }
                    col += columns;
                }
            }
            cells.push(line);
        }
        while cells.len() < usize::from(snapshot.height) {
            cells.push(vec![(" ".to_owned(), 0); width]);
        }

        let lines: Vec<String> = cells
            .iter()
            .map(|row| {
                let text: String = row.iter().map(|(text, _)| text.as_str()).collect();
                text.trim_end().to_owned()
            })
            .collect();

        let style_of = |id: u32| -> Style {
            snapshot
                .styles
                .get(usize::try_from(id).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or_default()
        };
        let base = dominant_style(&cells, &style_of);
        let spans = styled_spans(&cells, &style_of, base);

        let row_styles: Vec<u64> = cells
            .iter()
            .map(|row| {
                use std::hash::{Hash as _, Hasher as _};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                for (_, style) in row {
                    visible_style(style_of(*style)).hash(&mut hasher);
                }
                hasher.finish()
            })
            .collect();
        // The first look has nothing to compare with: no row "changed".
        let changed_rows = lines
            .iter()
            .enumerate()
            .filter(|(index, line)| !previous.is_empty() && previous.get(*index) != Some(*line))
            .map(|(index, _)| row_number(index))
            .collect::<Vec<_>>();
        let restyled_rows = row_styles
            .iter()
            .enumerate()
            .filter(|(index, style)| {
                !previous_styles.is_empty()
                    && previous.get(*index) == lines.get(*index)
                    && previous_styles.get(*index) != Some(*style)
            })
            .map(|(index, _)| row_number(index))
            .collect();

        let protocol = snapshot.protocol;
        Self {
            cols: snapshot.width,
            rows: snapshot.height,
            lines,
            cursor_row: snapshot.cursor.row.saturating_add(1),
            cursor_col: snapshot.cursor.col.saturating_add(1),
            cursor_visible: snapshot.cursor.visible,
            alternate_screen: snapshot.mode == "alternate",
            modes: Modes {
                mouse: mouse_mode_name(protocol.mouse_mode()).to_owned(),
                mouse_encoding: match protocol.mouse_encoding() {
                    bmux_terminal_grid::MouseProtocolEncoding::Sgr => "sgr",
                    bmux_terminal_grid::MouseProtocolEncoding::Utf8 => "utf8",
                    bmux_terminal_grid::MouseProtocolEncoding::Default => "x10",
                }
                .to_owned(),
                bracketed_paste: protocol.bracketed_paste,
                application_cursor_keys: protocol.application_cursor,
                application_keypad: protocol.application_keypad,
            },
            base_style: describe_style(base),
            spans,
            title: None,
            changed_rows,
            restyled_rows,
            row_styles,
        }
    }

    /// Text of the screen, one row per line.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// The text found at a 1-based row and column, for click reports.
    pub fn word_at(&self, row: u16, col: u16) -> Option<String> {
        let line = self.lines.get(usize::from(row.checked_sub(1)?))?;
        let chars: Vec<char> = line.chars().collect();
        let index = usize::from(col.checked_sub(1)?);
        if index >= chars.len() || chars[index].is_whitespace() {
            return None;
        }
        let start = chars[..index]
            .iter()
            .rposition(|c| c.is_whitespace())
            .map_or(0, |p| p + 1);
        let end = chars[index..]
            .iter()
            .position(|c| c.is_whitespace())
            .map_or(chars.len(), |p| index + p);
        Some(chars[start..end].iter().collect())
    }

    /// Where `needle` appears: 1-based (row, col) of each match.
    pub fn find(&self, needle: &str) -> Vec<(u16, u16)> {
        let mut found = Vec::new();
        for (index, line) in self.lines.iter().enumerate() {
            let mut from = 0;
            while let Some(position) = line[from..].find(needle) {
                let byte = from + position;
                let col = bmux_cell_geometry::display_width(&line[..byte]) + 1;
                found.push((row_number(index), u16::try_from(col).unwrap_or(u16::MAX)));
                from = byte + needle.len().max(1);
                if from >= line.len() {
                    break;
                }
            }
        }
        found
    }
}

fn row_number(index: usize) -> u16 {
    u16::try_from(index + 1).unwrap_or(u16::MAX)
}

const fn mouse_mode_name(mode: bmux_terminal_grid::MouseProtocolMode) -> &'static str {
    use bmux_terminal_grid::MouseProtocolMode;
    match mode {
        MouseProtocolMode::None => "off",
        MouseProtocolMode::Press => "press",
        MouseProtocolMode::PressRelease => "press-release",
        MouseProtocolMode::ButtonMotion => "drag",
        MouseProtocolMode::AnyMotion => "motion",
    }
}

/// The style covering the most cells: a themed app's background is its
/// "normal", not something to report on every row. A blank cell shows only
/// its background, so it counts as that background alone.
fn dominant_style(cells: &[Vec<(String, u32)>], style_of: &impl Fn(u32) -> Style) -> Style {
    let mut counts: HashMap<Style, usize> = HashMap::new();
    for row in cells {
        for (text, style) in row {
            let style = visible_style(style_of(*style));
            let look = if text.trim().is_empty() {
                background_look(style)
            } else {
                style
            };
            *counts.entry(look).or_default() += 1;
        }
    }
    // Ties go to the default style.
    counts
        .into_iter()
        .max_by_key(|(style, count)| (*count, *style == Style::default()))
        .map_or_else(Style::default, |(style, _)| style)
}

/// What a blank cell shows of a style: its background, nothing else.
fn background_look(style: Style) -> Style {
    let bg = effective_bg(style);
    Style {
        bg,
        ..Style::default()
    }
}

/// The parts of a style that change how a cell looks.
fn visible_style(style: Style) -> Style {
    Style {
        hidden: false,
        ..style
    }
}

fn styled_spans(
    cells: &[Vec<(String, u32)>],
    style_of: &impl Fn(u32) -> Style,
    base: Style,
) -> Vec<Span> {
    let mut spans = Vec::new();
    for (row_index, row) in cells.iter().enumerate() {
        let mut col = 0;
        while col < row.len() {
            let style = visible_style(style_of(row[col].1));
            if same_look(style, base, &row[col].0) {
                col += 1;
                continue;
            }
            let start = col;
            let mut text = String::new();
            while col < row.len() {
                let next = visible_style(style_of(row[col].1));
                if next != style {
                    break;
                }
                text.push_str(&row[col].0);
                col += 1;
            }
            let trimmed = text.trim();
            let highlight = paints_background(style, base);
            // A run of blanks only matters when it paints a background (a bar or selection).
            if trimmed.is_empty() && !highlight {
                continue;
            }
            // Colored text spans its glyphs, not the blanks around them.
            let (first, end) = if highlight {
                (start, col)
            } else {
                let glyph = |index: &usize| !row[*index].0.trim().is_empty();
                let first = (start..col).find(glyph).unwrap_or(start);
                let last = (start..col).rev().find(glyph).unwrap_or(col - 1);
                (first, last + 1)
            };
            spans.push(Span {
                row: row_number(row_index),
                last_row: row_number(row_index),
                col: u16::try_from(first + 1).unwrap_or(u16::MAX),
                width: u16::try_from(end - first).unwrap_or(u16::MAX),
                text: trimmed.to_owned(),
                style: describe_difference(style, base),
                highlight,
            });
        }
    }
    merge_vertical(spans)
}

/// Merge spans repeated on consecutive rows (borders, columns of markers) into
/// one, keeping reading order by first row.
fn merge_vertical(spans: Vec<Span>) -> Vec<Span> {
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    let mut open: HashMap<(u16, u16, String, String), usize> = HashMap::new();
    for span in spans {
        let key = (span.col, span.width, span.text.clone(), span.style.clone());
        if let Some(&index) = open.get(&key)
            && merged[index].last_row + 1 == span.row
        {
            merged[index].last_row = span.row;
            continue;
        }
        open.insert(key, merged.len());
        merged.push(span);
    }
    merged
}

/// Whether a cell looks like the base style: blanks only differ by background.
fn same_look(style: Style, base: Style, text: &str) -> bool {
    if style == base {
        return true;
    }
    let blank = text.trim().is_empty();
    blank && !paints_background(style, base)
}

fn paints_background(style: Style, base: Style) -> bool {
    style.inverse != base.inverse || effective_bg(style) != effective_bg(base)
}

fn effective_bg(style: Style) -> Option<Color> {
    if style.inverse { style.fg } else { style.bg }
}

/// A short description of a style: `default`, or its attributes and colors.
pub fn describe_style(style: Style) -> String {
    describe_difference(style, Style::default())
}

/// The attributes and colors where `style` differs from `base`.
fn describe_difference(style: Style, base: Style) -> String {
    let mut parts = Vec::new();
    for (on, was, name) in [
        (style.inverse, base.inverse, "inverse"),
        (style.bold, base.bold, "bold"),
        (style.dim, base.dim, "dim"),
        (style.italic, base.italic, "italic"),
        (style.underline, base.underline, "underline"),
        (style.strike, base.strike, "strike"),
    ] {
        if on && !was {
            parts.push(name.to_owned());
        } else if !on && was {
            parts.push(format!("no-{name}"));
        }
    }
    if style.fg != base.fg {
        parts.push(format!("fg={}", color_name(style.fg)));
    }
    if style.bg != base.bg {
        parts.push(format!("bg={}", color_name(style.bg)));
    }
    if parts.is_empty() {
        "default".to_owned()
    } else {
        parts.join(" ")
    }
}

fn color_name(color: Option<Color>) -> String {
    const NAMES: [&str; 16] = [
        "black",
        "red",
        "green",
        "yellow",
        "blue",
        "magenta",
        "cyan",
        "white",
        "bright-black",
        "bright-red",
        "bright-green",
        "bright-yellow",
        "bright-blue",
        "bright-magenta",
        "bright-cyan",
        "bright-white",
    ];
    match color {
        None => "default".to_owned(),
        Some(Color::Indexed(index)) => NAMES
            .get(usize::from(index))
            .map_or_else(|| format!("color{index}"), |name| (*name).to_owned()),
        Some(Color::Rgb { r, g, b }) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

/// How much of the screen a report shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenDetail {
    /// Every row (blank runs collapsed), changed rows marked.
    Full,
    /// Only rows that changed since the previous observation.
    Changes,
    /// No rows: a one-line summary.
    None,
}

/// Most highlighted spans (backgrounds, inverse) a report lists.
const HIGHLIGHT_LIMIT: usize = 16;
/// Most other styled spans (colored or bold text) a report lists.
const TEXT_STYLE_LIMIT: usize = 8;

/// Render an observation for an agent to read.
pub fn render(observation: &Observation, detail: ScreenDetail, all_spans: bool) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "screen {}x{} | cursor row {} col {}{}",
        observation.cols,
        observation.rows,
        observation.cursor_row,
        observation.cursor_col,
        if observation.cursor_visible { "" } else { " (hidden)" }
    );
    if observation.alternate_screen {
        out.push_str(" | alternate screen");
    }
    if observation.modes.mouse != "off" {
        let _ = write!(out, " | mouse {}", observation.modes.mouse);
    }
    if let Some(title) = &observation.title
        && !title.is_empty()
    {
        let _ = write!(out, " | title {title:?}");
    }
    out.push('\n');

    match detail {
        ScreenDetail::None => {}
        ScreenDetail::Full => render_rows(&mut out, observation, |_| true),
        ScreenDetail::Changes => {
            if observation.changed_rows.is_empty() && observation.restyled_rows.is_empty() {
                out.push_str("(no rows changed)\n");
            } else {
                render_rows(&mut out, observation, |row| {
                    observation.changed_rows.contains(&row)
                        || observation.restyled_rows.contains(&row)
                });
            }
        }
    }

    if detail != ScreenDetail::None && !observation.spans.is_empty() {
        let visible = |span: &&Span| match detail {
            ScreenDetail::Changes => (span.row..=span.last_row).any(|row| {
                observation.changed_rows.contains(&row) || observation.restyled_rows.contains(&row)
            }),
            _ => true,
        };
        let highlights: Vec<&Span> = observation
            .spans
            .iter()
            .filter(|span| span.highlight)
            .filter(visible)
            .collect();
        let others: Vec<&Span> = observation
            .spans
            .iter()
            .filter(|span| !span.highlight)
            .filter(visible)
            .collect();
        let (highlight_limit, other_limit) = if all_spans {
            (usize::MAX, usize::MAX)
        } else {
            (HIGHLIGHT_LIMIT, TEXT_STYLE_LIMIT)
        };
        if !highlights.is_empty() || !others.is_empty() {
            let _ = writeln!(out, "styled (differs from {}):", observation.base_style);
        }
        for span in highlights.iter().take(highlight_limit) {
            render_span(&mut out, span);
        }
        for span in others.iter().take(other_limit) {
            render_span(&mut out, span);
        }
        let hidden_highlights = highlights.len().saturating_sub(highlight_limit);
        let hidden_others = others.len().saturating_sub(other_limit);
        if hidden_highlights + hidden_others > 0 {
            let _ = writeln!(
                out,
                "  ... {} more highlighted and {} more colored/bold spans (--styles lists all)",
                hidden_highlights, hidden_others
            );
        }
    }
    out
}

fn render_span(out: &mut String, span: &Span) {
    let text = if span.text.is_empty() {
        format!("<{} blank cells>", span.width)
    } else {
        format!("{:?}", span.text)
    };
    let rows = if span.last_row > span.row {
        format!("rows {}-{}", span.row, span.last_row)
    } else {
        format!("row {}", span.row)
    };
    let _ = writeln!(
        out,
        "  {rows} col {}-{}: {text} [{}]",
        span.col,
        span.col + span.width.saturating_sub(1),
        span.style
    );
}

fn render_rows(out: &mut String, observation: &Observation, include: impl Fn(u16) -> bool) {
    let digits = observation.lines.len().to_string().len();
    // A run of blank rows prints as one line, marked when any of them changed.
    let mut blank: Option<(usize, bool)> = None;
    let flush = |out: &mut String, start: usize, end: usize, changed: bool| {
        let marker = if changed { '*' } else { ' ' };
        if end - start >= 2 {
            let _ = writeln!(out, "{marker}{:>digits$}| (rows {}-{} blank)", "", start + 1, end);
        } else {
            let _ = writeln!(out, "{marker}{:>digits$}|", start + 1);
        }
    };
    for (index, line) in observation.lines.iter().enumerate() {
        let row = row_number(index);
        let changed = observation.changed_rows.contains(&row);
        if (!include(row) || !line.is_empty())
            && let Some((start, any_changed)) = blank.take()
        {
            flush(out, start, index, any_changed);
        }
        if !include(row) {
            continue;
        }
        if line.is_empty() {
            let entry = blank.get_or_insert((index, false));
            entry.1 |= changed;
            continue;
        }
        let marker = if changed {
            '*'
        } else if observation.restyled_rows.contains(&row) {
            '~'
        } else {
            ' '
        };
        let _ = writeln!(out, "{marker}{row:>digits$}|{line}");
    }
    if let Some((start, any_changed)) = blank {
        flush(out, start, observation.lines.len(), any_changed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_terminal_grid::{GridLimits, TerminalGridStream};

    fn observe(bytes: &[u8], previous: &[String]) -> Observation {
        let mut stream = TerminalGridStream::new(20, 5, GridLimits::default()).unwrap();
        stream.process(bytes);
        let snapshot = stream.snapshot(0, 5);
        Observation::from_snapshot(&snapshot, previous, &[])
    }

    #[test]
    fn borders_repeated_down_rows_merge_into_one_span() {
        let obs = observe(b"\x1b[1m|\x1b[0m a\r\n\x1b[1m|\x1b[0m b\r\n\x1b[1m|\x1b[0m c", &[]);
        assert_eq!(obs.spans.len(), 1);
        assert_eq!((obs.spans[0].row, obs.spans[0].last_row), (1, 3));
        assert!(render(&obs, ScreenDetail::Full, false).contains("rows 1-3 col 1-1: \"|\" [bold]"));
    }

    #[test]
    fn colored_text_spans_only_its_glyphs() {
        let obs = observe(b"\x1b[31m  error   \x1b[0m", &[]);
        let span = &obs.spans[0];
        assert_eq!((span.col, span.width, span.text.as_str()), (3, 5, "error"));
    }

    #[test]
    fn rows_cursor_and_changes_are_one_based() {
        let first = observe(b"hello\r\nworld", &[]);
        assert!(first.changed_rows.is_empty());
        assert_eq!(first.lines[0], "hello");
        assert_eq!(first.lines[1], "world");
        assert_eq!((first.cursor_row, first.cursor_col), (2, 6));
        let second = observe(b"hello\r\nthere", &first.lines);
        assert_eq!(second.changed_rows, vec![2]);
    }

    #[test]
    fn a_moved_selection_marks_rows_restyled() {
        let mut stream = TerminalGridStream::new(20, 5, GridLimits::default()).unwrap();
        stream.process(b"\x1b[7mone\x1b[0m\r\ntwo");
        let first = Observation::from_snapshot(&stream.snapshot(0, 5), &[], &[]);
        stream.process(b"\x1b[Hone\r\n\x1b[7mtwo\x1b[0m");
        let second =
            Observation::from_snapshot(&stream.snapshot(0, 5), &first.lines, &first.row_styles);
        assert!(second.changed_rows.is_empty());
        assert_eq!(second.restyled_rows, vec![1, 2]);
        let text = render(&second, ScreenDetail::Changes, false);
        assert!(text.contains("~1|one") && text.contains("~2|two"), "{text}");
    }

    #[test]
    fn selection_bar_is_reported_against_the_dominant_style() {
        let obs = observe(b"  one\r\n\x1b[7m  two  \x1b[0m\r\n  three", &[]);
        assert_eq!(obs.spans.len(), 1);
        let span = &obs.spans[0];
        assert_eq!((span.row, span.col, span.width), (2, 1, 7));
        assert_eq!(span.text, "two");
        assert_eq!(span.style, "inverse");
    }

    #[test]
    fn themed_background_is_not_reported_everywhere() {
        let obs = observe(
            b"\x1b[44m\x1b[2J\x1b[Hmenu\r\n\x1b[41mERR\x1b[44m",
            &[],
        );
        assert_eq!(obs.base_style, "bg=blue");
        assert_eq!(obs.spans.len(), 1);
        assert_eq!(obs.spans[0].text, "ERR");
        assert_eq!(obs.spans[0].style, "bg=red");
    }

    #[test]
    fn blank_cells_count_only_their_background() {
        // Like vim's `~` filler: a few colored glyphs, many blank cells after them.
        let obs = observe(b"text\r\n\x1b[94m~    \r\n~    \r\n~    \x1b[0m", &[]);
        assert_eq!(obs.base_style, "default");
        assert!(obs.spans.iter().all(|span| span.text != "text"));
    }

    #[test]
    fn modes_follow_the_program() {
        let obs = observe(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?25l", &[]);
        assert!(obs.alternate_screen);
        assert_eq!(obs.modes.mouse, "press-release");
        assert!(obs.modes.bracketed_paste);
        assert!(!obs.cursor_visible);
    }

    #[test]
    fn find_reports_display_columns() {
        let obs = observe("名前 ok".as_bytes(), &[]);
        assert_eq!(obs.find("ok"), vec![(1, 6)]);
    }

    #[test]
    fn rendering_collapses_blank_rows_and_marks_changes() {
        let obs = observe(b"top", &["before".to_owned()]);
        let text = render(&obs, ScreenDetail::Full, false);
        assert!(text.contains("*1|top"), "{text}");
        assert!(text.contains("(rows 2-5 blank)"), "{text}");
    }
}
