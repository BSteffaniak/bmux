//! Generic picker/palette frame layout and chrome.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::prelude::{Line, Widget};
use bmux_tui::style::{Color, Modifier, Style};

/// Picker overlay placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerFramePlacement {
    /// Center the picker in the available area.
    Center,
    /// Place the picker around the upper third of the available area.
    UpperThird,
    /// Place the picker around the lower third of the available area.
    LowerThird,
    /// Place the picker at an explicit top-left point, clamped to the available area.
    Anchored(Point),
}

/// Behavior/layout policy for [`PickerFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PickerFramePolicy {
    /// Render outer panel chrome.
    pub chrome: bool,
    /// Fill the outer picker background.
    pub background: bool,
    /// Reserve a header row when header content is configured.
    pub header: bool,
    /// Reserve an input row.
    pub input: bool,
    /// Reserve a footer/status row when footer content is configured.
    pub footer: bool,
    /// Outer margin from the containing area.
    pub margin: Insets,
    /// Padding between panel chrome and inner content.
    pub padding: Insets,
    /// Minimum picker size.
    pub min_size: Size,
    /// Maximum picker size.
    pub max_size: Size,
    /// Picker placement.
    pub placement: PickerFramePlacement,
}

impl PickerFramePolicy {
    /// Bare layout with no chrome/background and no required input/footer rows.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            chrome: false,
            background: false,
            header: false,
            input: false,
            footer: false,
            margin: Insets::all(0),
            padding: Insets::all(0),
            min_size: Size::new(1, 1),
            max_size: Size::new(u16::MAX, u16::MAX),
            placement: PickerFramePlacement::Center,
        }
    }

    /// Command-palette style frame with chrome, background, header, input, and footer.
    #[must_use]
    pub const fn palette() -> Self {
        Self {
            chrome: true,
            background: true,
            header: true,
            input: true,
            footer: true,
            margin: Insets::all(2),
            padding: Insets::all(1),
            min_size: Size::new(20, 6),
            max_size: Size::new(72, 14),
            placement: PickerFramePlacement::UpperThird,
        }
    }

    /// Return this policy with placement changed.
    #[must_use]
    pub const fn placement(mut self, placement: PickerFramePlacement) -> Self {
        self.placement = placement;
        self
    }

    /// Return this policy with max size changed.
    #[must_use]
    pub const fn max_size(mut self, max_size: Size) -> Self {
        self.max_size = max_size;
        self
    }
}

impl Default for PickerFramePolicy {
    fn default() -> Self {
        Self::palette()
    }
}

/// Visual styles for [`PickerFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickerFrameStyles {
    /// Border/title style.
    pub border: Style,
    /// Background fill style.
    pub background: Style,
    /// Header fallback style.
    pub header: Style,
    /// Input-row fallback style.
    pub input: Style,
    /// List area fallback style.
    pub list: Style,
    /// Footer fallback style.
    pub footer: Style,
}

impl Default for PickerFrameStyles {
    fn default() -> Self {
        Self {
            border: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            background: Style::new().bg(Color::Black),
            header: Style::new().fg(Color::BrightWhite).bg(Color::Black),
            input: Style::new().fg(Color::White).bg(Color::Black),
            list: Style::new().fg(Color::White).bg(Color::Black),
            footer: Style::new().fg(Color::BrightBlack).bg(Color::Black),
        }
    }
}

/// Computed picker frame layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickerFrameLayout {
    /// Full picker panel area.
    pub panel: Rect,
    /// Inner content area after chrome/padding.
    pub inner: Rect,
    /// Optional header row.
    pub header: Option<Rect>,
    /// Optional input row.
    pub input: Option<Rect>,
    /// List/content area.
    pub list: Rect,
    /// Optional footer row.
    pub footer: Option<Rect>,
}

/// Generic picker/palette frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerFrame<'a> {
    title: Option<&'a str>,
    header: Option<Line>,
    footer: Option<Line>,
    policy: PickerFramePolicy,
    styles: PickerFrameStyles,
}

impl<'a> PickerFrame<'a> {
    /// Create a picker frame.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            title: None,
            header: None,
            footer: None,
            policy: PickerFramePolicy::palette(),
            styles: PickerFrameStyles {
                border: Style::new(),
                background: Style::new(),
                header: Style::new(),
                input: Style::new(),
                list: Style::new(),
                footer: Style::new(),
            },
        }
    }

    /// Set title.
    #[must_use]
    pub const fn title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }

    /// Set header content.
    #[must_use]
    pub fn header(mut self, header: impl Into<Line>) -> Self {
        self.header = Some(header.into());
        self
    }

    /// Set footer/status content.
    #[must_use]
    pub fn footer(mut self, footer: impl Into<Line>) -> Self {
        self.footer = Some(footer.into());
        self
    }

    /// Set policy.
    #[must_use]
    pub const fn policy(mut self, policy: PickerFramePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: PickerFrameStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Compute layout inside `area`.
    #[must_use]
    pub fn layout(&self, area: Rect) -> PickerFrameLayout {
        let available = area.inset(self.policy.margin);
        let panel = place_rect(
            available,
            desired_size(available, self.policy),
            self.policy.placement,
        );
        let inner = self.panel().inner_area(panel);
        let mut y = inner.y;
        let header =
            (self.policy.header && self.header.is_some() && y < inner.bottom()).then(|| {
                let rect = Rect::new(inner.x, y, inner.width, 1);
                y = y.saturating_add(1);
                rect
            });
        if header.is_some() && y < inner.bottom() {
            y = y.saturating_add(1);
        }
        let input = (self.policy.input && y < inner.bottom()).then(|| {
            let rect = Rect::new(inner.x, y, inner.width, 1);
            y = y.saturating_add(1);
            rect
        });
        if input.is_some() && y < inner.bottom() {
            y = y.saturating_add(1);
        }
        let footer_height =
            u16::from(self.policy.footer && self.footer.is_some() && y < inner.bottom());
        let list_bottom = inner.bottom().saturating_sub(footer_height);
        let list = Rect::new(inner.x, y, inner.width, list_bottom.saturating_sub(y));
        let footer = (footer_height > 0).then_some(Rect::new(inner.x, list_bottom, inner.width, 1));
        PickerFrameLayout {
            panel,
            inner,
            header,
            input,
            list,
            footer,
        }
    }

    /// Render frame chrome and configured header/footer text, returning layout.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) -> PickerFrameLayout {
        let layout = self.layout(area);
        self.panel().render(layout.panel, frame);
        if let (Some(header_area), Some(header)) = (layout.header, &self.header) {
            frame.write_line_with_fallback_style(header_area, header, self.styles.header);
        }
        if layout.input.is_some() {
            frame.fill(layout.input.unwrap_or_default(), " ", self.styles.input);
        }
        frame.fill(layout.list, " ", self.styles.list);
        if let (Some(footer_area), Some(footer)) = (layout.footer, &self.footer) {
            frame.write_line_with_fallback_style(footer_area, footer, self.styles.footer);
        }
        layout
    }

    fn panel(&self) -> Panel {
        let mut panel = Panel::new();
        if self.policy.chrome {
            panel = panel.border(Border::single().style(self.styles.border));
            if let Some(title) = self.title {
                panel = panel.title(title);
            }
        }
        if self.policy.background {
            panel = panel.background(self.styles.background);
        }
        panel.padding(self.policy.padding)
    }
}

impl Default for PickerFrame<'_> {
    fn default() -> Self {
        Self::new()
    }
}

fn desired_size(area: Rect, policy: PickerFramePolicy) -> Size {
    let width = policy
        .max_size
        .width
        .min(area.width)
        .max(policy.min_size.width.min(area.width));
    let height = policy
        .max_size
        .height
        .min(area.height)
        .max(policy.min_size.height.min(area.height));
    Size::new(width, height)
}

fn place_rect(area: Rect, size: Size, placement: PickerFramePlacement) -> Rect {
    let width = size.width.min(area.width);
    let height = size.height.min(area.height);
    let x = match placement {
        PickerFramePlacement::Center
        | PickerFramePlacement::UpperThird
        | PickerFramePlacement::LowerThird => {
            area.x.saturating_add(area.width.saturating_sub(width) / 2)
        }
        PickerFramePlacement::Anchored(point) => point.x.min(area.right().saturating_sub(width)),
    };
    let y = match placement {
        PickerFramePlacement::Center => area
            .y
            .saturating_add(area.height.saturating_sub(height) / 2),
        PickerFramePlacement::UpperThird => area
            .y
            .saturating_add(area.height.saturating_sub(height) / 3),
        PickerFramePlacement::LowerThird => area
            .y
            .saturating_add(area.height.saturating_sub(height) * 2 / 3),
        PickerFramePlacement::Anchored(point) => point.y.min(area.bottom().saturating_sub(height)),
    };
    Rect::new(x, y, width, height)
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`PickerFrameStyles`].
    #[must_use]
    pub fn picker_frame_styles(self) -> PickerFrameStyles {
        PickerFrameStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for PickerFrameStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Raised);
        Self {
            border: theme.focused,
            background: theme.surfaces.raised,
            header: theme.text.add_modifier(bmux_tui::style::Modifier::BOLD),
            input: theme.text,
            list: theme.text,
            footer: theme.muted,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Insets, Point, Rect, Size};

    use super::{PickerFrame, PickerFramePlacement, PickerFramePolicy};

    #[test]
    fn computes_full_palette_layout() {
        let frame = PickerFrame::new()
            .title("Commands")
            .header("Type to filter")
            .footer("enter select")
            .policy(PickerFramePolicy::palette().max_size(Size::new(40, 10)));

        let layout = frame.layout(Rect::new(0, 0, 80, 24));

        assert_eq!(layout.panel, Rect::new(20, 5, 40, 10));
        assert_eq!(layout.inner, Rect::new(22, 7, 36, 6));
        assert_eq!(layout.header, Some(Rect::new(22, 7, 36, 1)));
        assert_eq!(layout.input, Some(Rect::new(22, 9, 36, 1)));
        assert_eq!(layout.list, Rect::new(22, 11, 36, 1));
        assert_eq!(layout.footer, Some(Rect::new(22, 12, 36, 1)));
    }

    #[test]
    fn supports_no_input_or_footer_layout() {
        let frame = PickerFrame::new()
            .header("Header")
            .policy(PickerFramePolicy {
                input: false,
                footer: false,
                max_size: Size::new(20, 6),
                margin: Insets::all(0),
                placement: PickerFramePlacement::Center,
                ..PickerFramePolicy::palette()
            });

        let layout = frame.layout(Rect::new(0, 0, 40, 10));

        assert_eq!(layout.input, None);
        assert_eq!(layout.footer, None);
        assert_eq!(layout.list.height, 0);
    }

    #[test]
    fn bare_layout_uses_whole_area_without_chrome() {
        let frame = PickerFrame::new().policy(PickerFramePolicy::bare());

        let layout = frame.layout(Rect::new(1, 2, 12, 4));

        assert_eq!(layout.panel, Rect::new(1, 2, 12, 4));
        assert_eq!(layout.inner, Rect::new(1, 2, 12, 4));
        assert_eq!(layout.header, None);
        assert_eq!(layout.input, None);
        assert_eq!(layout.footer, None);
        assert_eq!(layout.list, Rect::new(1, 2, 12, 4));
    }

    #[test]
    fn anchored_layout_is_clamped_to_area() {
        let frame = PickerFrame::new().policy(PickerFramePolicy {
            max_size: Size::new(10, 5),
            min_size: Size::new(10, 5),
            margin: Insets::all(0),
            placement: PickerFramePlacement::Anchored(Point::new(100, 100)),
            ..PickerFramePolicy::palette()
        });

        let layout = frame.layout(Rect::new(0, 0, 40, 10));

        assert_eq!(layout.panel, Rect::new(30, 5, 10, 5));
    }

    #[test]
    fn tiny_area_degrades_without_invalid_rects() {
        let frame = PickerFrame::new().header("H").footer("F");

        let layout = frame.layout(Rect::new(0, 0, 4, 3));

        assert!(layout.panel.width <= 4);
        assert!(layout.panel.height <= 3);
        assert!(layout.inner.width <= layout.panel.width);
        assert!(layout.inner.height <= layout.panel.height);
    }

    #[test]
    fn render_writes_configured_chrome_and_text() {
        let picker = PickerFrame::new()
            .title("Pick")
            .header("Header")
            .footer("Footer")
            .policy(PickerFramePolicy {
                margin: Insets::all(0),
                max_size: Size::new(20, 10),
                ..PickerFramePolicy::palette()
            });
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 12));
        let mut frame = Frame::new(&mut buffer);

        picker.render(Rect::new(0, 0, 40, 12), &mut frame);
        assert!(
            frame
                .buffer()
                .row_symbols(0)
                .is_some_and(|row| row.contains("Pick"))
        );
        assert!(
            frame
                .buffer()
                .row_symbols(2)
                .is_some_and(|row| row.contains("Header"))
        );
        assert!(
            frame
                .buffer()
                .row_symbols(7)
                .is_some_and(|row| row.contains("Footer"))
        );
    }
}
