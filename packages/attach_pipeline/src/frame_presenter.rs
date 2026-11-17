//! Frames painted straight into a `bmux_tui` terminal by the attach renderer.
//!
//! A [`FramePresenter`] owns the terminal a client presents through: the attach renderer paints
//! each frame into it in place, over the frame it shows (see [`crate::canvas`]), and the terminal
//! encodes only what changed. Styles, the cursor, synchronized output and row moves come from the
//! terminal, not the renderer. Graphics protocols keep their own encoders: their bytes are
//! written through the terminal's output hooks, removals before the frame's cells and placements
//! after them.
//!
//! Every client presents this way. A headless client's terminal encodes nothing; it keeps the
//! frame and its change for observers, which mirror the change the terminal made (see
//! [`FramePresenter::shown_changes`]).

use std::cell::RefCell;
use std::io::Write;

use bmux_tui::buffer::Buffer;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::pointer::PointerShapeCapability;
use bmux_tui::terminal::{FrameChanges, ScrollRegionAcceleration, Terminal, TryDrawError};

use crate::canvas::{Canvas, FrameOutput};
use crate::projection::ProjectionFailure;

/// Presents frames through a `bmux_tui` terminal.
pub struct FramePresenter {
    terminal: Terminal<Vec<u8>>,
    /// A frame was presented since the presenter was made or last forgot: the next frame can be
    /// painted over it rather than over a blank screen.
    shown: bool,
    /// What the last call to [`Self::present`] did to the frame shown before it.
    last: Last,
}

impl Default for FramePresenter {
    fn default() -> Self {
        Self::new()
    }
}

/// What presenting a frame did to the frame shown before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Last {
    /// Nothing is known (nothing presented, a frame not presented).
    Unknown,
    /// Nothing changed.
    Unchanged,
    /// It was painted in place over it: the terminal holds what changed.
    Over,
    /// It was painted whole, over a blank screen.
    Full,
}

/// What the last frame presented changed in the frame shown before it, so the same frame can be
/// kept elsewhere (an observer's copy) by applying the change instead of painting it again.
pub enum ShownChanges<'presenter> {
    /// Nothing is known: no frame was presented.
    Unknown,
    /// Nothing changed.
    Unchanged,
    /// The frame's moves and changed cells, and the cells it shows.
    Changed {
        cells: &'presenter Buffer,
        changes: FrameChanges<'presenter>,
    },
    /// The frame was painted whole: these are its cells.
    All { cells: &'presenter Buffer },
}

/// What presenting a frame produced.
#[derive(Debug)]
pub enum Presented {
    /// Bytes to write to the host, the whole frame (empty for a terminal that encodes nothing).
    Frame(Vec<u8>),
    /// Nothing changed on the host.
    Unchanged,
    /// The frame holds what a terminal frame cannot show; nothing was presented, and the next
    /// frame repaints in full.
    Unsupported(ProjectionFailure),
}

/// A frame to present.
#[derive(Clone, Copy, Debug)]
pub struct FrameSpec {
    /// The frame starts from a blank screen, as the renderer's full frames do.
    pub full: bool,
    pub size: (u16, u16),
    /// The host has left and right margins (DECSLRM), so a narrower pane scrolls inside them.
    pub margins: bool,
    /// The host takes graphics protocol bytes; otherwise graphics are placed as images.
    pub graphics: bool,
}

/// A presented frame: what the render returned, what presenting produced, and what the frame
/// produced beside its cells.
#[derive(Debug)]
pub struct PresentedFrame<T> {
    pub result: T,
    pub presented: Presented,
    pub output: FrameOutput,
}

/// Why a frame was not painted: the render failed, or the frame cannot be shown.
enum Abandoned<E> {
    Render(E),
    Unsupported,
}

impl FramePresenter {
    /// A presenter that has shown nothing yet: its first frame repaints in full.
    #[must_use]
    pub fn new() -> Self {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 0, 0));
        // Attach clients present pointer shapes themselves.
        terminal.set_pointer_capability(PointerShapeCapability::Unsupported);
        Self {
            terminal,
            shown: false,
            last: Last::Unknown,
        }
    }

    /// Select whether frames are wrapped in a synchronized update (DEC mode 2026).
    pub const fn set_synchronized_output(&mut self, enabled: bool) {
        self.terminal.set_synchronized_output(enabled);
    }

    /// Select whether frames are encoded (the default); a headless client's are not, and are
    /// kept only for observers.
    pub const fn set_encoding(&mut self, enabled: bool) {
        self.terminal.set_encoding(enabled);
    }

    /// Paint a frame with `paint`, over the last frame presented, and return the bytes that
    /// change the host to match.
    ///
    /// The frame counts as shown once its bytes are returned; when they do not reach the host,
    /// [`Self::forget`] makes the next frame repaint in full.
    ///
    /// # Errors
    /// Returns the render's error; nothing is presented, and the next frame repaints in full.
    pub fn present<T, E>(
        &mut self,
        spec: &FrameSpec,
        paint: impl FnOnce(&mut Canvas<'_, '_, '_>) -> Result<T, E>,
    ) -> Result<PresentedFrame<T>, E> {
        self.last = Last::Unknown;
        let (cols, rows) = spec.size;
        self.terminal.resize(Rect::new(0, 0, cols, rows));
        if spec.full {
            self.terminal.reset();
        }
        self.terminal
            .set_scroll_region_acceleration(if spec.margins {
                ScrollRegionAcceleration::AutoWithMargins
            } else {
                ScrollRegionAcceleration::Auto
            });
        self.terminal.writer_mut().clear();
        let output = RefCell::new(if spec.graphics {
            FrameOutput::with_graphics()
        } else {
            FrameOutput::default()
        });
        let mut result = None;
        let drawn = self.terminal.try_draw_over_with_output_hooks(
            |cx| {
                let mut output = output.borrow_mut();
                let value = paint(&mut Canvas::new(cx, &mut output)).map_err(Abandoned::Render)?;
                result = Some(value);
                if output.rejection().is_some() {
                    return Err(Abandoned::Unsupported);
                }
                Ok(output.has_graphics())
            },
            |writer| {
                if let Some(graphics) = output.borrow().graphics() {
                    writer.write_all(&graphics.before)?;
                }
                Ok(false)
            },
            |writer, _, _| {
                let output = output.borrow();
                let Some(graphics) = output
                    .graphics()
                    .filter(|graphics| !graphics.after.is_empty())
                else {
                    return Ok(());
                };
                // Placing images moves the cursor: keep the frame's.
                writer.write_all(b"\x1b7")?;
                writer.write_all(&graphics.after)?;
                writer.write_all(b"\x1b8")
            },
        );
        let mut output = output.into_inner();
        let presented = match drawn {
            Ok(Some(_)) => {
                self.shown = true;
                self.last = if self.terminal.last_changes().is_some() {
                    Last::Over
                } else {
                    Last::Full
                };
                Presented::Frame(std::mem::take(self.terminal.writer_mut()))
            }
            Ok(None) => {
                self.last = Last::Unchanged;
                Presented::Unchanged
            }
            Err(TryDrawError::Rejected(Abandoned::Render(error))) => {
                self.forget();
                return Err(error);
            }
            Err(TryDrawError::Rejected(Abandoned::Unsupported)) => {
                self.forget();
                Presented::Unsupported(output.take_rejection().unwrap_or(
                    ProjectionFailure::Unsupported {
                        reason: "frame rejected",
                    },
                ))
            }
            Err(TryDrawError::Io(_)) => {
                // Frames are written into memory; a failing hook leaves nothing shown.
                self.forget();
                Presented::Unchanged
            }
        };
        let Some(result) = result else {
            unreachable!("a frame that was painted returned the render's result");
        };
        Ok(PresentedFrame {
            result,
            presented,
            output,
        })
    }

    /// Forget what was presented, so the next frame repaints in full: after its bytes failed to
    /// reach the host, or after the host was changed some other way.
    pub fn forget(&mut self) {
        self.terminal.reset();
        self.shown = false;
        self.last = Last::Unknown;
    }

    /// Whether a frame was presented since the presenter was made or last forgot. Until one is,
    /// frames must be full: they are painted over a blank screen.
    #[must_use]
    pub const fn shown(&self) -> bool {
        self.shown
    }

    /// The cells of the last frame presented, unless forgotten.
    #[must_use]
    pub fn shown_cells(&self) -> Option<&Buffer> {
        self.shown
            .then(|| self.terminal.retained_buffer())
            .flatten()
    }

    /// The scrolls the terminal applied presenting the last frame: rows it moved on the host
    /// instead of writing them again.
    #[must_use]
    pub fn applied_scrolls(&self) -> &[bmux_tui::terminal::AppliedScroll] {
        self.terminal.applied_scrolls()
    }

    /// What the last frame presented changed in the one shown before it.
    #[must_use]
    pub fn shown_changes(&self) -> ShownChanges<'_> {
        match (
            self.last,
            self.terminal.retained_buffer(),
            self.terminal.last_changes(),
        ) {
            (Last::Unchanged, _, _) => ShownChanges::Unchanged,
            (Last::Over, Some(cells), Some(changes)) => ShownChanges::Changed { cells, changes },
            (Last::Full, Some(cells), _) => ShownChanges::All { cells },
            _ => ShownChanges::Unknown,
        }
    }
}

/// Bring a copy of the frames a presenter shows up to date.
///
/// Paints over the copy of the frame shown before the last one the change the last one made.
/// Returns `false` when the change is unknown (no frame was presented), leaving the copy as it
/// was.
pub fn paint_shown_changes(cx: &mut PaintCx<'_, '_>, shown: &ShownChanges<'_>) -> bool {
    match shown {
        ShownChanges::Unknown => false,
        ShownChanges::Unchanged => true,
        ShownChanges::Changed { cells, changes } => {
            for (area, rows) in changes.moves() {
                cx.move_cells(LocalRect::terminal(area), rows);
            }
            for (y, run) in changes.runs() {
                cx.copy_cells(cells, y, run);
            }
            true
        }
        ShownChanges::All { cells } => {
            let area = cells.area();
            for y in area.y..area.bottom() {
                cx.copy_cells(cells, y, area.x..area.right());
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FramePresenter, FrameSpec, Presented, ShownChanges};
    use crate::AttachCursorState;
    use crate::projection::ProjectionFailure;
    use bmux_tui::style::Style;

    fn spec(full: bool) -> FrameSpec {
        FrameSpec {
            full,
            size: (10, 2),
            margins: false,
            graphics: false,
        }
    }

    fn present(
        presenter: &mut FramePresenter,
        full: bool,
        text: Option<(u16, &str)>,
        cursor: Option<AttachCursorState>,
    ) -> Presented {
        presenter
            .present(&spec(full), |canvas| {
                if let Some((y, text)) = text {
                    canvas.text(0, y, text, Style::new());
                }
                canvas.set_cursor(cursor);
                Ok::<_, ()>(())
            })
            .unwrap()
            .presented
    }

    #[test]
    fn frames_write_what_changed_and_nothing_when_nothing_did() {
        let mut presenter = FramePresenter::new();
        assert!(!presenter.shown());
        let cursor = Some(AttachCursorState {
            x: 1,
            y: 1,
            visible: true,
        });
        let Presented::Frame(first) = present(&mut presenter, true, Some((0, "hello")), cursor)
        else {
            panic!("the first frame is presented");
        };
        assert!(String::from_utf8_lossy(&first).contains("hello"));
        assert!(presenter.shown());
        assert!(matches!(
            presenter.shown_changes(),
            ShownChanges::All { .. }
        ));
        assert!(matches!(
            present(&mut presenter, false, None, cursor),
            Presented::Unchanged
        ));
        assert!(matches!(presenter.shown_changes(), ShownChanges::Unchanged));
        let Presented::Frame(edit) = present(&mut presenter, false, Some((1, "world")), cursor)
        else {
            panic!("an edit is presented");
        };
        let edit = String::from_utf8_lossy(&edit);
        assert!(
            edit.contains("world") && !edit.contains("hello"),
            "{edit:?}"
        );
        assert!(matches!(
            presenter.shown_changes(),
            ShownChanges::Changed { .. }
        ));
        let moved = Some(AttachCursorState {
            x: 2,
            y: 0,
            visible: true,
        });
        assert!(matches!(
            present(&mut presenter, false, None, moved),
            Presented::Frame(_)
        ));
    }

    #[test]
    fn rejected_frames_present_nothing_and_forgetting_repaints() {
        let mut presenter = FramePresenter::new();
        assert!(matches!(
            present(&mut presenter, true, Some((0, "kept")), None),
            Presented::Frame(_)
        ));
        let rejected = presenter
            .present(&spec(false), |canvas| {
                canvas.text(0, 1, "half", Style::new());
                canvas.reject(ProjectionFailure::Budget {
                    resource: "operations",
                    used: 2,
                    limit: 1,
                });
                Ok::<_, ()>(())
            })
            .unwrap();
        assert!(matches!(
            rejected.presented,
            Presented::Unsupported(ProjectionFailure::Budget { .. })
        ));
        assert!(!presenter.shown(), "a rejected frame leaves nothing shown");
        let Presented::Frame(repaint) = present(&mut presenter, true, Some((0, "kept")), None)
        else {
            panic!("a full frame after a rejection is presented");
        };
        assert!(String::from_utf8_lossy(&repaint).contains("kept"));
        assert!(
            presenter
                .present(&spec(false), |_| Err::<(), _>("render failed"))
                .is_err(),
            "a failed render presents nothing"
        );
        assert!(!presenter.shown());
    }
}
