//! What the attach renderer paints into: a `bmux_tui` frame, and the frame's side outputs.
//!
//! The renderer paints straight into the frame a presenter is drawing (the cells a host's
//! terminal shows, or a headless client's copy of them), with a terminal's semantics: a text run
//! replaces the cells it covers with exactly its style, colors it leaves unset being the
//! terminal's defaults. Nothing is recorded and replayed. Beside the cells, a frame carries what
//! the cells cannot hold: its images (the terminal's image scene, which the presenter writes for
//! the host and observers mirror), and the reason a frame cannot be shown at all.

use bmux_tui::geometry::{Point, Rect};
use bmux_tui::image::ImagePlacement;
use bmux_tui::paint::{LocalRect, PaintCx, SizedCell};
use bmux_tui::style::{Color, Style};
use bmux_tui::text::{Line, Span};

use crate::AttachCursorState;
use crate::projection::{PaintOperation, ProjectionFailure};

/// What a frame produces beside its cells.
#[derive(Debug, Default)]
pub struct FrameOutput {
    /// The frame is shown on a host terminal (not only kept for observers).
    host: bool,
    /// Why the frame cannot be shown, if it cannot.
    rejection: Option<ProjectionFailure>,
}

impl FrameOutput {
    /// Output for a frame shown on a host terminal.
    #[must_use]
    pub fn for_host() -> Self {
        Self {
            host: true,
            ..Self::default()
        }
    }

    /// Why the frame cannot be shown, if it cannot.
    #[must_use]
    pub const fn rejection(&self) -> Option<&ProjectionFailure> {
        self.rejection.as_ref()
    }

    /// Take why the frame cannot be shown.
    pub const fn take_rejection(&mut self) -> Option<ProjectionFailure> {
        self.rejection.take()
    }
}

/// A frame being painted: its paint context and its side outputs.
pub struct Canvas<'canvas, 'frame, 'buffer> {
    cx: &'canvas mut PaintCx<'frame, 'buffer>,
    output: &'canvas mut FrameOutput,
}

impl<'canvas, 'frame, 'buffer> Canvas<'canvas, 'frame, 'buffer> {
    /// A canvas over `cx`, collecting side outputs in `output`.
    pub const fn new(
        cx: &'canvas mut PaintCx<'frame, 'buffer>,
        output: &'canvas mut FrameOutput,
    ) -> Self {
        Self { cx, output }
    }

    /// The frame's paint context.
    pub const fn cx(&mut self) -> &mut PaintCx<'frame, 'buffer> {
        self.cx
    }

    /// The frame's area.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.cx.clip()
    }

    /// Fill `rect` with blanks in `style`.
    pub fn fill(&mut self, rect: Rect, style: Style) {
        self.cx.fill(LocalRect::terminal(rect), " ", style);
    }

    /// Paint `text` from column `x` of row `y`, replacing each cell it covers with exactly
    /// `style`: colors it leaves unset are the terminal's defaults, as the reset before every run
    /// of terminal output makes them.
    pub fn text(&mut self, x: u16, y: u16, text: &str, style: Style) {
        let explicit = |color: Option<Color>| Some(color.unwrap_or(Color::Default));
        let style = Style {
            fg: explicit(style.fg),
            bg: explicit(style.bg),
            ..style
        };
        if text.is_ascii() {
            // Each ASCII byte is one narrow cell: no grapheme segmentation needed.
            self.cx.replace_cells(
                i32::from(x),
                i64::from(y),
                (0..text.len()).map(|index| SizedCell {
                    symbol: &text[index..=index],
                    width: 1,
                    style,
                }),
            );
            return;
        }
        if text.chars().all(standalone_char) {
            // Every character is a grapheme of its own (box drawing, most symbols).
            self.cx.replace_cells(
                i32::from(x),
                i64::from(y),
                text.char_indices().map(|(start, ch)| SizedCell {
                    symbol: &text[start..start + ch.len_utf8()],
                    width: u8::try_from(unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1))
                        .unwrap_or(1),
                    style,
                }),
            );
            return;
        }
        let width = bmux_plugin::render_text_width_u16(text);
        self.cx.write_line(
            LocalRect::new(i32::from(x), i64::from(y), width, 1),
            &Line::from_spans(vec![Span::styled(text, style)]),
        );
    }

    /// Paint a run of cells from column `x` of row `y`, each a grapheme with the columns it
    /// occupies and its whole style.
    pub fn cells<'symbol>(
        &mut self,
        x: u16,
        y: u16,
        cells: impl IntoIterator<Item = SizedCell<'symbol>>,
    ) {
        self.cx.replace_cells(i32::from(x), i64::from(y), cells);
    }

    /// Move the cells of `area` by `rows` (positive up), as a terminal scroll inside it moves
    /// them; the rows it exposes are blank.
    pub fn scroll(&mut self, area: Rect, rows: i64) {
        self.cx.move_cells(LocalRect::terminal(area), rows);
    }

    /// Contribute an image to the frame: the terminal's image scene keeps it, the presenter's
    /// image compositor writes it for the host, and observers mirror it.
    pub fn image(&mut self, placement: ImagePlacement) {
        self.cx
            .push_image(bmux_tui::image::ImageContribution::Present(placement));
    }

    /// Remove the image `key` (one contributed as persistent) from the frame's image scene.
    pub fn remove_image(&mut self, key: bmux_tui::image::ImageKey) {
        self.cx
            .push_image(bmux_tui::image::ImageContribution::Remove(key));
    }

    /// Declare this frame's images its complete image set (see
    /// [`bmux_tui::paint::PaintCx::declare_images`]).
    pub const fn declare_images(&mut self) {
        self.cx.declare_images();
    }

    /// Occlude images contributed so far by an opaque surface over `area`, leaving cells alone.
    pub fn occlude_images(&mut self, area: Rect) {
        self.cx.occlude_images(LocalRect::terminal(area));
    }

    /// The frame cannot be shown: the first reason is kept. A pane deferred inside an
    /// unfinished synchronized update is not a reason; it keeps the cells it showed.
    pub const fn reject(&mut self, failure: ProjectionFailure) {
        if !matches!(failure, ProjectionFailure::Deferred { .. }) && self.output.rejection.is_none()
        {
            self.output.rejection = Some(failure);
        }
    }

    /// Whether the frame was rejected.
    #[must_use]
    pub const fn rejected(&self) -> bool {
        self.output.rejection.is_some()
    }

    /// Whether the frame is shown on a host terminal (not only kept for observers).
    #[must_use]
    pub const fn for_host(&self) -> bool {
        self.output.host
    }

    /// Show the cursor the frame leaves, if any.
    pub fn set_cursor(&mut self, cursor: Option<AttachCursorState>) {
        if let Some(cursor) = cursor {
            self.cx
                .set_cursor(Point::new(cursor.x, cursor.y), cursor.visible);
        }
    }

    /// Paint retained operations (an extension's output kept for replay).
    pub fn replay(&mut self, operations: &[PaintOperation]) {
        for operation in operations {
            match operation {
                PaintOperation::Text { x, y, text, style } => self.text(*x, *y, text, *style),
                PaintOperation::Fill { rect, style } => self.fill(*rect, *style),
                PaintOperation::Rejected(failure) => self.reject(failure.clone()),
            }
        }
    }
}

/// Whether `ch` is always a grapheme of its own, one or two columns wide: not zero-width (a
/// joiner, a combining mark, a variation selector) and not one of the characters that combine with
/// a neighbour into one grapheme (regional indicators, emoji skin-tone modifiers).
fn standalone_char(ch: char) -> bool {
    !matches!(ch, '\u{1f1e6}'..='\u{1f1ff}' | '\u{1f3fb}'..='\u{1f3ff}')
        && matches!(unicode_width::UnicodeWidthChar::width(ch), Some(1 | 2))
}

/// Paint into `buffer` through a canvas (a frame painted over what `buffer` holds), for callers
/// that keep their own cells: tests, benchmarks.
pub fn paint_on<T>(
    buffer: &mut bmux_tui::buffer::Buffer,
    output: &mut FrameOutput,
    paint: impl FnOnce(&mut Canvas<'_, '_, '_>) -> T,
) -> T {
    let mut frame = bmux_tui::frame::Frame::new(buffer);
    let mut cx = PaintCx::new(&mut frame);
    paint(&mut Canvas::new(&mut cx, output))
}
