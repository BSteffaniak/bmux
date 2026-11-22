//! Painting helpers over bmux's [`PaintCx`] and components.
//!
//! They split areas, and draw framed panes, text and hit regions. Every view paints through a [`Ui`], which knows the theme, what
//! the pointer is over, and records where things were drawn so events can reach them.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use bmux_tui::component::Component;
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span};
use bmux_tui_components::pane::{Pane, PaneComponent, PaneState, PaneStyles};

use crate::theme::Theme;

/// A share of an axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// Exactly this many cells.
    Fixed(u16),
    /// A share of what the fixed sizes leave, by weight.
    Fill(u16),
}

/// Split `length` cells into `sizes`.
#[must_use]
pub fn split(length: u16, sizes: &[Size]) -> Vec<u16> {
    let fixed: u16 = sizes
        .iter()
        .map(|size| match size {
            Size::Fixed(cells) => *cells,
            Size::Fill(_) => 0,
        })
        .fold(0, u16::saturating_add);
    let weights: u32 = sizes
        .iter()
        .map(|size| match size {
            Size::Fill(weight) => u32::from(*weight),
            Size::Fixed(_) => 0,
        })
        .sum();
    let free = length.saturating_sub(fixed);
    let mut lengths: Vec<u16> = Vec::with_capacity(sizes.len());
    let mut given = 0_u16;
    let mut seen = 0_u32;
    for size in sizes {
        let cells = match size {
            Size::Fixed(cells) => (*cells).min(length.saturating_sub(given)),
            Size::Fill(weight) => {
                seen += u32::from(*weight);
                // The running share, so rounding never loses a cell.
                let share = |weight: u32| {
                    (u32::from(free) * weight)
                        .checked_div(weights)
                        .map_or(0, |cells| u16::try_from(cells).unwrap_or(free))
                };
                let end = share(seen);
                let start = share(seen - u32::from(*weight));
                end - start
            }
        };
        given = given.saturating_add(cells);
        lengths.push(cells);
    }
    lengths
}

/// `area` split top to bottom.
#[must_use]
pub fn rows(area: Rect, sizes: &[Size]) -> Vec<Rect> {
    let mut y = area.y;
    split(area.height, sizes)
        .into_iter()
        .map(|height| {
            let rect = Rect::new(area.x, y, area.width, height);
            y = y.saturating_add(height);
            rect
        })
        .collect()
}

/// `area` split left to right.
#[must_use]
pub fn columns(area: Rect, sizes: &[Size]) -> Vec<Rect> {
    let mut x = area.x;
    split(area.width, sizes)
        .into_iter()
        .map(|width| {
            let rect = Rect::new(x, area.y, width, area.height);
            x = x.saturating_add(width);
            rect
        })
        .collect()
}

/// `area` without `cells` on every side.
#[must_use]
pub const fn inset(area: Rect, cells: u16) -> Rect {
    Rect::new(
        area.x.saturating_add(cells),
        area.y.saturating_add(cells),
        area.width.saturating_sub(cells * 2),
        area.height.saturating_sub(cells * 2),
    )
}

/// The row `index` of `area` (empty when outside it).
#[must_use]
pub const fn row(area: Rect, index: u16) -> Rect {
    if index >= area.height {
        return Rect::new(area.x, area.y, 0, 0);
    }
    Rect::new(area.x, area.y + index, area.width, 1)
}

const fn local(area: Rect) -> LocalRect {
    LocalRect::new(area.x as i32, area.y as i64, area.width, area.height)
}

/// What a frame recorded for events to use: where each named thing was drawn.
#[derive(Debug, Default)]
pub struct Areas(RefCell<BTreeMap<String, Rect>>);

impl Areas {
    /// Forget the last frame's areas.
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    /// Record where `name` was drawn.
    pub fn set(&self, name: impl Into<String>, area: Rect) {
        self.0.borrow_mut().insert(name.into(), area);
    }

    /// Where `name` was drawn last frame.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Rect> {
        self.0.borrow().get(name).copied()
    }
}

/// Paints one frame; see the [module documentation](self).
pub struct Ui<'a, 'b, 'c> {
    /// The paint context.
    pub cx: &'a mut PaintCx<'b, 'c>,
    /// The theme.
    pub theme: &'a Theme,
    /// The hit region the pointer is over.
    pub hover: Option<&'a str>,
    /// Where things were drawn.
    pub areas: &'a Areas,
    /// The animation clock: frames since the dashboard started.
    pub tick: u64,
}

impl Ui<'_, '_, '_> {
    /// Fill `area` with `style`.
    pub fn fill(&mut self, area: Rect, style: Style) {
        if !area.is_empty() {
            self.cx.fill(local(area), " ", style);
        }
    }

    /// Write `line` at the start of `area`'s first row, clipped to it.
    pub fn line(&mut self, area: Rect, line: &Line) {
        if !area.is_empty() {
            self.cx
                .write_line(local(Rect::new(area.x, area.y, area.width, 1)), line);
        }
    }

    /// Write `lines` down `area`, one per row, clipped to it.
    pub fn lines(&mut self, area: Rect, lines: &[Line]) {
        for (index, line) in lines.iter().enumerate() {
            let Ok(index) = u16::try_from(index) else {
                break;
            };
            if index >= area.height {
                break;
            }
            self.line(row(area, index), line);
        }
    }

    /// Write one cell.
    pub fn cell(&mut self, x: u16, y: u16, symbol: &str, style: Style) {
        self.cx.set_cell(i32::from(x), i64::from(y), symbol, style);
    }

    /// Make `area` respond to the pointer as `id`.
    pub fn hit(&mut self, id: impl Into<String>, area: Rect) {
        self.hit_as(id, area, HitRole::Action);
    }

    /// Make `area` respond to the pointer as `id`, in `role`.
    pub fn hit_as(&mut self, id: impl Into<String>, area: Rect, role: HitRole) {
        if area.is_empty() {
            return;
        }
        self.cx.push_hit(
            HitRegion::new(id.into(), area)
                .role(role)
                .hoverable(true)
                .layer(1),
        );
    }

    /// Whether the pointer is over `id`.
    #[must_use]
    pub fn hovered(&self, id: &str) -> bool {
        self.hover == Some(id)
    }

    /// Whether the pointer is over `id` or something inside it (`id.…`).
    #[must_use]
    pub fn hovered_within(&self, id: &str) -> bool {
        self.hover.is_some_and(|hover| {
            hover == id
                || hover
                    .strip_prefix(id)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    }

    /// Paint `component` into `area`, recording it as `name`.
    pub fn component(&mut self, name: &str, component: &impl Component, area: Rect) {
        if area.is_empty() {
            return;
        }
        self.areas.set(name, area);
        self.cx.paint_component(component, area);
    }

    /// A framed pane titled `title` (with `right` status on its title row) over `area`;
    /// returns the area inside its border. `accent` draws its border in the accent color,
    /// for the pane in focus.
    pub fn pane(
        &mut self,
        id: &str,
        title: Line,
        right: Option<Line>,
        area: Rect,
        accent: bool,
    ) -> Rect {
        if area.width < 3 || area.height < 3 {
            return Rect::new(area.x, area.y, 0, 0);
        }
        let border = if accent {
            self.theme.accent()
        } else {
            self.theme.faint()
        };
        let mut pane = Pane::new().title(title).styles(PaneStyles {
            background: Some(self.theme.surface()),
            border,
            focused_border: self.theme.accent(),
        });
        if let Some(right) = right {
            pane = pane.right_title(right);
        }
        let state = Cell::new(PaneState::new(area));
        let component = PaneComponent::new(id.to_owned(), pane, &state, TextBlock::new(""));
        self.component(id, &component, area);
        inset(area, 1)
    }

    /// A clickable label: `text` in `style`, raised while hovered, registered as `id`.
    /// Returns the cells it took.
    pub fn button(&mut self, id: &str, x: u16, y: u16, text: &str, style: Style) -> u16 {
        let width = u16::try_from(bmux_tui::text::Line::raw(text).width()).unwrap_or(u16::MAX);
        let area = Rect::new(x, y, width, 1);
        let style = if self.hovered(id) {
            style.patch(self.theme.raised())
        } else {
            style
        };
        self.line(area, &one(Span::styled(text.to_owned(), style)));
        self.hit(id, area);
        width
    }
}

fn one(span: Span) -> Line {
    Line::from_spans(vec![span])
}

/// `text` truncated to `width` cells with an ellipsis.
#[must_use]
pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// A horizontal bar of `width` cells filled to `fraction`, with eighth-cell precision.
#[must_use]
pub fn bar(fraction: f64, width: usize) -> String {
    const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )] // display
    let eighths = (fraction.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let mut out = "█".repeat(eighths / 8);
    out.push_str(EIGHTHS[eighths % 8]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_give_fixed_cells_and_share_the_rest_without_losing_any() {
        assert_eq!(
            split(10, &[Size::Fixed(3), Size::Fill(1), Size::Fill(1)]),
            [3, 3, 4]
        );
        assert_eq!(split(5, &[Size::Fixed(8), Size::Fill(1)]), [5, 0]);
        assert_eq!(split(7, &[Size::Fill(1), Size::Fill(2)]), [2, 5]);
    }

    #[test]
    fn bars_and_truncation_fit_their_width() {
        assert_eq!(bar(0.5, 4), "██");
        assert_eq!(bar(1.0, 2), "██");
        assert_eq!(bar(0.06, 2), "▏");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("ab", 4), "ab");
    }
}
