//! Opaque modal frame components.
//!
//! [`ModalFrame`] is the preferred foundation for overlay dialogs. It keeps the
//! low-level [`bmux_tui::composition::Surface`] primitive flexible while making modal
//! surfaces opaque by default so underlying content cannot bleed through blank
//! rows or short text lines.

use bmux_tui::chrome::Border;
use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, Element, EventCx, LayoutCx, LayoutId,
    LayoutNode, LogicalSize,
};
use bmux_tui::composition::{SizeBox, Stack, Surface};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::paint::PaintCx;
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
    /// Create a modal theme from caller-owned semantic styles.
    #[must_use]
    pub const fn new(
        background: Style,
        border: Style,
        title: Style,
        text: Style,
        muted: Style,
        focused: Style,
    ) -> Self {
        Self {
            background,
            border,
            title,
            text,
            muted,
            focused,
            scrim: None,
        }
    }

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
    /// Center horizontally and place the modal around the lower third.
    LowerThird,
    /// Place the modal at an explicit top-left point, clamped to the parent.
    Anchored(Point),
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

    /// Create modal sizing constraints with equal min and max size.
    #[must_use]
    pub const fn fixed(size: Size, margin: Insets) -> Self {
        Self {
            min: size,
            max: size,
            margin,
        }
    }

    /// Return this sizing with the maximum size changed.
    #[must_use]
    pub const fn max_size(mut self, max: Size) -> Self {
        self.max = max;
        self
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
        let x = match self.placement {
            ModalPlacement::Centered | ModalPlacement::UpperThird | ModalPlacement::LowerThird => {
                parent
                    .x
                    .saturating_add(parent.width.saturating_sub(size.width) / 2)
            }
            ModalPlacement::Anchored(point) => {
                point.x.min(parent.right().saturating_sub(size.width))
            }
        };
        let y = match self.placement {
            ModalPlacement::Centered => parent
                .y
                .saturating_add(parent.height.saturating_sub(size.height) / 2),
            ModalPlacement::UpperThird => parent
                .y
                .saturating_add(parent.height.saturating_sub(size.height) / 3),
            ModalPlacement::LowerThird => parent
                .y
                .saturating_add(parent.height.saturating_sub(size.height) * 2 / 3),
            ModalPlacement::Anchored(point) => {
                point.y.min(parent.bottom().saturating_sub(size.height))
            }
        };
        Rect::new(x, y, size.width, size.height)
    }

    /// Return the resolved modal content area for a parent area.
    #[must_use]
    pub fn content_area(&self, parent: Rect) -> Rect {
        self.panel_area(parent)
            .inset(self.border.sides.insets())
            .inset(self.padding)
    }

    /// Return this modal's visual theme.
    #[must_use]
    pub const fn theme(&self) -> ModalTheme {
        self.theme
    }
}

struct ComponentRef<'a>(&'a Element<'a>);

impl Component for ComponentRef<'_> {
    fn revision(&self) -> ComponentRevision {
        self.0.revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.0.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.0.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.0.event(event, layout, cx)
    }
}

#[derive(Clone, Copy)]
struct Scrim(Style);

impl Component for Scrim {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        LayoutNode::leaf(
            LayoutId::new("modal-scrim"),
            LogicalSize::new(
                constraints.max_width(),
                constraints
                    .max_height()
                    .unwrap_or_else(|| constraints.min_height()),
            ),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        cx.fill(
            bmux_tui::paint::LocalRect::new(
                0,
                0,
                layout.size.width,
                u16::try_from(layout.size.height).unwrap_or(u16::MAX),
            ),
            " ",
            self.0,
        );
    }
}

struct ModalPlacementComponent<'a> {
    id: LayoutId,
    placement: ModalPlacement,
    margin: Insets,
    child: Element<'a>,
}

impl<'a> ModalPlacementComponent<'a> {
    fn new(
        id: impl Into<LayoutId>,
        placement: ModalPlacement,
        margin: Insets,
        child: impl Component + 'a,
    ) -> Self {
        Self {
            id: id.into(),
            placement,
            margin,
            child: Element::new(child),
        }
    }
}

impl Component for ModalPlacementComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        self.child.revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let size = constraints.constrain(LogicalSize::new(
            constraints.max_width(),
            constraints
                .max_height()
                .unwrap_or_else(|| constraints.min_height()),
        ));
        let child = self.child.layout(
            constraints.inset(
                self.margin.horizontal(),
                usize::from(self.margin.vertical()),
            ),
            cx,
        );
        let remaining_x = size.width.saturating_sub(child.size.width);
        let remaining_y = size.height.saturating_sub(child.size.height);
        let (x, y) = match self.placement {
            ModalPlacement::Centered => (remaining_x / 2, remaining_y / 2),
            ModalPlacement::UpperThird => (remaining_x / 2, remaining_y / 3),
            ModalPlacement::LowerThird => (remaining_x / 2, remaining_y.saturating_mul(2) / 3),
            ModalPlacement::Anchored(point) => (
                point.x.min(remaining_x),
                usize::from(point.y).min(remaining_y),
            ),
        };
        LayoutNode::with_children(self.id.clone(), size, vec![ChildLayout::new(x, y, child)])
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let Some(child) = layout.children.first() else {
            return;
        };
        cx.with_child(
            i32::from(child.x),
            i64::try_from(child.y).unwrap_or(i64::MAX),
            bmux_tui::paint::LocalRect::new(
                0,
                0,
                child.node.size.width,
                u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
            ),
            |cx| self.child.paint(&child.node, cx),
        );
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(child) = layout.children.first() else {
            return EventOutcome::Ignored;
        };
        let clip = Rect::new(
            child.x,
            u16::try_from(child.y).unwrap_or(u16::MAX),
            child.node.size.width,
            u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
        );
        cx.with_transform(
            child.x,
            child.y,
            i32::from(child.x),
            i64::try_from(child.y).unwrap_or(i64::MAX),
            clip,
            |cx| self.child.event(event, &child.node, cx),
        )
    }
}

/// Canonical child-owning modal surface.
pub struct ModalFrameComponent<'a> {
    id: LayoutId,
    frame: ModalFrame,
    child: Element<'a>,
    chrome: bool,
}

impl<'a> ModalFrameComponent<'a> {
    /// Create a modal frame around one measurable child.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, frame: ModalFrame, child: impl Component + 'a) -> Self {
        Self {
            id: id.into(),
            frame,
            child: Element::new(child),
            chrome: true,
        }
    }

    /// Preserve modal geometry and opacity while leaving border decoration to the host.
    #[must_use]
    pub const fn chrome(mut self, chrome: bool) -> Self {
        self.chrome = chrome;
        self
    }

    fn tree(&self) -> Stack<'_> {
        let sizing = self.frame.sizing;
        let panel = Surface::new(ComponentRef(&self.child))
            .id(format!("{}.surface", self.id.as_str()))
            .background(self.frame.theme.background)
            .content_style(self.frame.theme.text)
            .border(self.frame.border.clone())
            .paint_border(self.chrome)
            .padding(self.frame.padding);
        let panel = SizeBox::new(panel)
            .id(format!("{}.size", self.id.as_str()))
            .min_width(sizing.min.width)
            .max_width(sizing.max.width)
            .min_height(usize::from(sizing.min.height))
            .max_height(usize::from(sizing.max.height));
        let placed = ModalPlacementComponent::new(
            format!("{}.placement", self.id.as_str()),
            self.frame.placement,
            sizing.margin,
            panel,
        );
        let mut stack = Stack::new().id(self.id.clone());
        if let Some(scrim) = self.frame.theme.scrim {
            stack = stack.child(Scrim(scrim));
        }
        stack.child(placed)
    }
}

impl Component for ModalFrameComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        self.tree().revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.tree().layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.tree().paint(layout, cx);
        let Some(title) = &self.frame.title else {
            return;
        };
        let surface_id = LayoutId::new(format!("{}.surface", self.id.as_str()));
        let Some(surface) = layout.find_logical_rect(&surface_id) else {
            return;
        };
        let width = surface.width.saturating_sub(2);
        if width == 0 {
            return;
        }
        cx.write_line(
            bmux_tui::paint::LocalRect::new(
                i32::from(surface.x.saturating_add(1)),
                i64::try_from(surface.y).unwrap_or(i64::MAX),
                width,
                1,
            ),
            &title.clone().with_fallback_style(self.frame.theme.title),
        );
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.tree().event(event, layout, cx)
    }
}

fn clamp_axis(available: u16, min: u16, max: u16) -> u16 {
    available.clamp(min.min(available), max)
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`ModalTheme`].
    #[must_use]
    pub fn modal_theme(self) -> ModalTheme {
        ModalTheme::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for ModalTheme {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Overlay);
        Self {
            background: theme.surfaces.overlay,
            border: theme.focused,
            title: theme.info.add_modifier(bmux_tui::style::Modifier::BOLD),
            text: theme.text,
            muted: theme.muted,
            focused: theme.focused,
            scrim: theme.surfaces.scrim,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ModalFrame, ModalFrameComponent, ModalPlacement, ModalSizing, ModalTheme};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx};
    use bmux_tui::composition::TextContent;
    use bmux_tui::event::Event;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Insets, Point, Rect, Size};
    use bmux_tui::paint::PaintCx;
    use bmux_tui::style::{Color, Style};

    #[test]
    fn component_composes_scrim_surface_and_child() {
        let theme = ModalTheme::dark(Color::Cyan).with_scrim(Style::new().bg(Color::BrightBlack));
        let frame = ModalFrame::new(ModalSizing::fixed(Size::new(12, 5), Insets::all(0)), theme)
            .title("Confirm")
            .padding(Insets::new(1, 1, 1, 1));
        let component = ModalFrameComponent::new(
            "confirm",
            frame,
            TextContent::new("Proceed?").id("confirm.body"),
        );
        let layout = component.layout(Constraints::new(30, 30, 10, Some(10)), &mut LayoutCx::new());
        assert_eq!(layout.size.width, 30);
        assert_eq!(layout.size.height, 10);
        assert!(layout.find(&"confirm.surface".into()).is_some());
        assert!(layout.find(&"confirm.body".into()).is_some());

        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 10));
        let mut terminal = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut terminal));
        assert_eq!(
            component.event(&Event::Tick, &layout, &mut EventCx::new(&layout)),
            bmux_tui::event::EventOutcome::Ignored
        );
        assert_eq!(
            terminal.buffer().get(Point::new(0, 0)).unwrap().style.bg,
            Some(Color::BrightBlack)
        );
        assert!((0..10).any(|row| {
            terminal
                .buffer()
                .row_symbols(row)
                .is_some_and(|symbols| symbols.contains("Proceed?"))
        }));
        assert!((0..10).any(|row| {
            terminal
                .buffer()
                .row_symbols(row)
                .is_some_and(|symbols| symbols.contains("Confirm"))
        }));
    }

    #[test]
    fn component_preserves_anchored_placement() {
        let frame = ModalFrame::new(
            ModalSizing::fixed(Size::new(10, 4), Insets::all(0)),
            ModalTheme::dark(Color::Green),
        )
        .placement(ModalPlacement::Anchored(Point::new(7, 3)));
        let component = ModalFrameComponent::new("anchored", frame, TextContent::new("body"));
        let layout = component.layout(Constraints::new(30, 30, 10, Some(10)), &mut LayoutCx::new());
        let surface = layout
            .find_logical_rect(&"anchored.surface".into())
            .expect("surface geometry");
        assert_eq!(surface.x, 7);
        assert_eq!(surface.y, 3);
    }

    #[test]
    fn modal_frame_fills_entire_panel_area_with_background() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 8));
        let mut frame = Frame::new(&mut buffer);
        let theme = ModalTheme::dark(Color::Cyan);
        let modal = ModalFrame::new(
            ModalSizing::new(Size::new(8, 4), Size::new(8, 4), Insets::all(0)),
            theme,
        );

        let component = ModalFrameComponent::new("modal", modal.clone(), TextContent::new(""));
        let layout = component.layout(
            Constraints::tight(frame.area().size()),
            &mut LayoutCx::new(),
        );
        component.paint(&layout, &mut PaintCx::new(&mut frame));

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
    fn lower_third_placement_uses_last_third_of_remaining_space() {
        let modal = ModalFrame::new(
            ModalSizing::fixed(Size::new(10, 6), Insets::all(0)),
            ModalTheme::dark(Color::Green),
        )
        .placement(ModalPlacement::LowerThird);

        assert_eq!(
            modal.panel_area(Rect::new(0, 0, 40, 21)),
            Rect::new(15, 10, 10, 6)
        );
    }

    #[test]
    fn anchored_placement_is_clamped_to_parent() {
        let modal = ModalFrame::new(
            ModalSizing::fixed(Size::new(10, 6), Insets::all(0)),
            ModalTheme::dark(Color::Green),
        )
        .placement(ModalPlacement::Anchored(Point::new(100, 100)));

        assert_eq!(
            modal.panel_area(Rect::new(0, 0, 40, 21)),
            Rect::new(30, 15, 10, 6)
        );
    }

    #[test]
    fn sizing_clamps_to_available_parent_area() {
        let modal = ModalFrame::new(
            ModalSizing::new(Size::new(20, 10), Size::new(60, 40), Insets::all(2)),
            ModalTheme::dark(Color::Green),
        );

        assert_eq!(
            modal.panel_area(Rect::new(0, 0, 12, 8)),
            Rect::new(2, 2, 8, 4)
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

        let component = ModalFrameComponent::new("modal", modal, TextContent::new(""));
        let layout = component.layout(
            Constraints::tight(frame.area().size()),
            &mut LayoutCx::new(),
        );
        component.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).expect("cell").style.bg,
            Some(Color::BrightBlack)
        );
    }
}
