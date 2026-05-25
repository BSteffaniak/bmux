//! Opaque modal frame components.
//!
//! [`ModalFrame`] is the preferred foundation for overlay dialogs. It keeps the
//! low-level [`bmux_tui::chrome::Panel`] primitive flexible while making modal
//! surfaces opaque by default so underlying content cannot bleed through blank
//! rows or short text lines.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::Widget;
use bmux_tui::style::{Color, Style};
use bmux_tui::text::Line;

/// Visual styles used by modal surfaces and their common child controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalTheme {
    /// Opaque style used to fill the modal panel area.
    pub background: Style,
    /// Border style for the modal panel.
    pub border: Style,
    /// Title style for modal titles.
    pub title: Style,
    /// Primary body text style.
    pub text: Style,
    /// Muted labels, descriptions, and hints.
    pub muted: Style,
    /// Focused or accented interactive element style.
    pub focused: Style,
    /// Optional full-parent scrim style rendered before the modal panel.
    pub scrim: Option<Style>,
}

impl ModalTheme {
    /// Create a default dark opaque modal theme using `accent` for focused
    /// chrome.
    #[must_use]
    pub const fn dark(accent: Color) -> Self {
        Self {
            background: Style::new().bg(Color::Black),
            border: Style::new().fg(accent).bg(Color::Black),
            title: Style::new().fg(accent).bg(Color::Black),
            text: Style::new().fg(Color::BrightWhite).bg(Color::Black),
            muted: Style::new().fg(Color::BrightBlack).bg(Color::Black),
            focused: Style::new().fg(accent).bg(Color::Black),
            scrim: None,
        }
    }

    /// Return this theme with a full-parent scrim style.
    #[must_use]
    pub const fn with_scrim(mut self, style: Style) -> Self {
        self.scrim = Some(style);
        self
    }
}

/// Modal placement within a parent rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalPlacement {
    /// Center the modal in both axes.
    Centered,
    /// Center horizontally and place the modal around the upper third.
    UpperThird,
}

/// Modal sizing constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalSizing {
    /// Smallest preferred modal size.
    pub min: Size,
    /// Largest preferred modal size.
    pub max: Size,
    /// Margin preserved around the modal before sizing is clamped.
    pub margin: Insets,
}

impl ModalSizing {
    /// Create modal sizing constraints.
    #[must_use]
    pub const fn new(min: Size, max: Size, margin: Insets) -> Self {
        Self { min, max, margin }
    }

    fn resolve_size(self, parent: Rect) -> Size {
        let available_width = parent.width.saturating_sub(self.margin.horizontal());
        let available_height = parent.height.saturating_sub(self.margin.vertical());
        Size::new(
            clamp_axis(available_width, self.min.width, self.max.width),
            clamp_axis(available_height, self.min.height, self.max.height),
        )
    }
}

/// An opaque modal panel frame with optional scrim, consistent sizing, and
/// reusable content-area calculation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalFrame {
    title: Option<Line>,
    border: Border,
    padding: Insets,
    sizing: ModalSizing,
    placement: ModalPlacement,
    theme: ModalTheme,
}

impl ModalFrame {
    /// Create a modal frame with the supplied sizing and visual theme.
    #[must_use]
    pub const fn new(sizing: ModalSizing, theme: ModalTheme) -> Self {
        Self {
            title: None,
            border: Border::rounded().style(theme.border),
            padding: Insets::new(1, 2, 1, 2),
            sizing,
            placement: ModalPlacement::Centered,
            theme,
        }
    }

    /// Set the modal title.
    #[must_use]
    pub fn title(mut self, title: impl Into<Line>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the modal border.
    #[must_use]
    pub const fn border(mut self, border: Border) -> Self {
        self.border = border;
        self
    }

    /// Set the modal panel padding.
    #[must_use]
    pub const fn padding(mut self, padding: Insets) -> Self {
        self.padding = padding;
        self
    }

    /// Set the modal placement.
    #[must_use]
    pub const fn placement(mut self, placement: ModalPlacement) -> Self {
        self.placement = placement;
        self
    }

    /// Return the resolved modal panel area for a parent area.
    #[must_use]
    pub fn panel_area(&self, parent: Rect) -> Rect {
        let size = self.sizing.resolve_size(parent);
        let x = parent
            .x
            .saturating_add(parent.width.saturating_sub(size.width) / 2);
        let y_offset = match self.placement {
            ModalPlacement::Centered => parent.height.saturating_sub(size.height) / 2,
            ModalPlacement::UpperThird => parent.height.saturating_sub(size.height) / 3,
        };
        Rect::new(
            x,
            parent.y.saturating_add(y_offset),
            size.width,
            size.height,
        )
    }

    /// Return the resolved modal content area for a parent area.
    #[must_use]
    pub fn content_area(&self, parent: Rect) -> Rect {
        self.panel().inner_area(self.panel_area(parent))
    }

    /// Render the modal scrim and opaque panel frame.
    pub fn render(&self, parent: Rect, frame: &mut Frame<'_>) {
        if let Some(scrim) = self.theme.scrim {
            frame.fill(parent, " ", scrim);
        }
        self.panel().render(self.panel_area(parent), frame);
    }

    /// Render one line inside this modal using the theme text style as an
    /// opaque fallback.
    pub fn render_line(&self, area: Rect, line: &Line, frame: &mut Frame<'_>) {
        frame.write_line_with_fallback_style(area, line, self.theme.text);
    }

    /// Return this modal's visual theme.
    #[must_use]
    pub const fn theme(&self) -> ModalTheme {
        self.theme
    }

    fn panel(&self) -> Panel {
        let mut panel = Panel::new()
            .border(self.border.clone())
            .padding(self.padding)
            .background(self.theme.background);
        if let Some(title) = &self.title {
            panel = panel.title(title.clone());
        }
        panel
    }
}

fn clamp_axis(available: u16, min: u16, max: u16) -> u16 {
    available.clamp(min.min(available), max)
}

#[cfg(test)]
mod tests {
    use super::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Insets, Point, Rect, Size};
    use bmux_tui::style::{Color, Style};

    #[test]
    fn modal_frame_fills_entire_panel_area_with_background() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 8));
        let mut frame = Frame::new(&mut buffer);
        let theme = ModalTheme::dark(Color::Cyan);
        let modal = ModalFrame::new(
            ModalSizing::new(Size::new(8, 4), Size::new(8, 4), Insets::all(0)),
            theme,
        );

        modal.render(frame.area(), &mut frame);

        let panel = modal.panel_area(frame.area());
        for y in panel.y..panel.bottom() {
            for x in panel.x..panel.right() {
                let cell = frame
                    .buffer()
                    .get(Point::new(x, y))
                    .expect("panel cell should exist");
                assert_eq!(cell.style.bg, Some(Color::Black));
            }
        }
    }

    #[test]
    fn content_area_accounts_for_border_and_padding() {
        let modal = ModalFrame::new(
            ModalSizing::new(Size::new(20, 10), Size::new(20, 10), Insets::all(0)),
            ModalTheme::dark(Color::Yellow),
        )
        .padding(Insets::new(1, 2, 3, 4));

        assert_eq!(
            modal.content_area(Rect::new(0, 0, 40, 20)),
            Rect::new(15, 7, 12, 4)
        );
    }

    #[test]
    fn upper_third_placement_uses_first_third_of_remaining_space() {
        let modal = ModalFrame::new(
            ModalSizing::new(Size::new(10, 6), Size::new(10, 6), Insets::all(0)),
            ModalTheme::dark(Color::Green),
        )
        .placement(ModalPlacement::UpperThird);

        assert_eq!(
            modal.panel_area(Rect::new(0, 0, 40, 21)),
            Rect::new(15, 5, 10, 6)
        );
    }

    #[test]
    fn scrim_fills_parent_area_when_present() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 5));
        let mut frame = Frame::new(&mut buffer);
        let theme = ModalTheme::dark(Color::Cyan).with_scrim(Style::new().bg(Color::BrightBlack));
        let modal = ModalFrame::new(
            ModalSizing::new(Size::new(4, 3), Size::new(4, 3), Insets::all(0)),
            theme,
        );

        modal.render(frame.area(), &mut frame);

        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).expect("cell").style.bg,
            Some(Color::BrightBlack)
        );
    }
}
