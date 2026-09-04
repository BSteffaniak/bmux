//! Generic picker/palette frame layout and chrome.

use std::hash::{Hash, Hasher};

use bmux_tui::chrome::Border;
use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, Element, EventCx, LayoutCx, LayoutId,
    LayoutNode, LogicalSize, combine_child_revisions,
};
use bmux_tui::composition::Surface;
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::Line;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PickerFrameLayout {
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

    fn resolved_layout(&self, area: Rect) -> PickerFrameLayout {
        let available = area.inset(self.policy.margin);
        let panel = place_rect(
            available,
            desired_size(available, self.policy),
            self.policy.placement,
        );
        let border = u16::from(self.policy.chrome);
        let inner = panel.inset(Insets::new(
            self.policy.padding.top.saturating_add(border),
            self.policy.padding.right.saturating_add(border),
            self.policy.padding.bottom.saturating_add(border),
            self.policy.padding.left.saturating_add(border),
        ));
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
}

/// Canonical child-owning picker frame composition.
pub struct PickerFrameComponent<'a> {
    id: LayoutId,
    frame: PickerFrame<'a>,
    input: Option<Element<'a>>,
    list: Element<'a>,
}

impl<'a> PickerFrameComponent<'a> {
    /// Create a picker frame around list content.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, frame: PickerFrame<'a>, list: impl Component + 'a) -> Self {
        Self {
            id: id.into(),
            frame,
            input: None,
            list: Element::new(list),
        }
    }

    /// Set optional input content. Its presence follows the frame input policy.
    #[must_use]
    pub fn input(mut self, input: impl Component + 'a) -> Self {
        self.input = Some(Element::new(input));
        self
    }

    fn panel_size(&self, constraints: Constraints) -> LogicalSize {
        let width = self
            .frame
            .policy
            .max_size
            .width
            .min(constraints.max_width())
            .max(
                self.frame
                    .policy
                    .min_size
                    .width
                    .min(constraints.max_width()),
            );
        let available_height = constraints
            .max_height()
            .unwrap_or_else(|| constraints.min_height());
        let height = usize::from(self.frame.policy.max_size.height)
            .min(available_height)
            .max(usize::from(self.frame.policy.min_size.height).min(available_height));
        LogicalSize::new(width, height)
    }

    fn panel_origin(&self, outer: LogicalSize, panel: LogicalSize) -> (u16, usize) {
        let margin = self.frame.policy.margin;
        let available_width = outer.width.saturating_sub(margin.horizontal());
        let available_height = outer.height.saturating_sub(usize::from(margin.vertical()));
        let remaining_x = available_width.saturating_sub(panel.width);
        let remaining_y = available_height.saturating_sub(panel.height);
        let x = match self.frame.policy.placement {
            PickerFramePlacement::Center
            | PickerFramePlacement::UpperThird
            | PickerFramePlacement::LowerThird => remaining_x / 2,
            PickerFramePlacement::Anchored(point) => point.x.min(remaining_x),
        };
        let y = match self.frame.policy.placement {
            PickerFramePlacement::Center => remaining_y / 2,
            PickerFramePlacement::UpperThird => remaining_y / 3,
            PickerFramePlacement::LowerThird => remaining_y.saturating_mul(2) / 3,
            PickerFramePlacement::Anchored(point) => usize::from(point.y).min(remaining_y),
        };
        (
            margin.left.saturating_add(x),
            usize::from(margin.top).saturating_add(y),
        )
    }

    fn local_layout(&self, size: LogicalSize) -> PickerFrameLayout {
        let mut frame = self.frame.clone();
        frame.policy.margin = Insets::all(0);
        frame.resolved_layout(Rect::new(
            0,
            0,
            size.width,
            u16::try_from(size.height).unwrap_or(u16::MAX),
        ))
    }

    /// The panel chrome as the core child-owning [`Surface`] container.
    ///
    /// Background, border, and padding belong to the surface that measures
    /// the panel rectangle; header, input, list, and footer rows are placed
    /// inside its content insets by [`PickerFrame::resolved_layout`].
    fn panel_surface(&self) -> Surface<'static> {
        let mut surface = Surface::new(EmptyContent)
            .id(format!("{}.surface", self.id.as_str()))
            .padding(self.frame.policy.padding);
        if self.frame.policy.background {
            surface = surface.background(self.frame.styles.background);
        }
        if self.frame.policy.chrome {
            surface = surface.border(Border::single().style(self.frame.styles.border));
        }
        surface
    }

    fn paint_chrome(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let Some(panel) = layout.children.first() else {
            return;
        };
        let local = self.local_layout(panel.node.size);
        let panel_height = u16::try_from(panel.node.size.height).unwrap_or(u16::MAX);
        cx.with_child(
            i32::from(panel.x),
            i64::try_from(panel.y).unwrap_or(i64::MAX),
            LocalRect::new(0, 0, panel.node.size.width, panel_height),
            |cx| {
                let surface = self.panel_surface();
                let surface_layout =
                    surface.layout(Constraints::tight(local.panel.size()), &mut LayoutCx::new());
                surface.paint(&surface_layout, cx);
                if self.frame.policy.chrome
                    && let Some(title) = self.frame.title
                {
                    let width = local.panel.width.saturating_sub(2);
                    if width > 0 {
                        cx.write_line_with_fallback_style(
                            LocalRect::new(
                                i32::from(local.panel.x.saturating_add(1)),
                                i64::from(local.panel.y),
                                width,
                                1,
                            ),
                            &Line::from(title),
                            self.frame.styles.border,
                        );
                    }
                }
                if let (Some(area), Some(header)) = (local.header, &self.frame.header) {
                    cx.write_line_with_fallback_style(
                        LocalRect::terminal(area),
                        header,
                        self.frame.styles.header,
                    );
                }
                if let Some(area) = local.input {
                    cx.fill(LocalRect::terminal(area), " ", self.frame.styles.input);
                }
                cx.fill(LocalRect::terminal(local.list), " ", self.frame.styles.list);
                if let (Some(area), Some(footer)) = (local.footer, &self.frame.footer) {
                    cx.write_line_with_fallback_style(
                        LocalRect::terminal(area),
                        footer,
                        self.frame.styles.footer,
                    );
                }
            },
        );
    }
}

/// Zero-size placeholder so the panel [`Surface`] measures only its own
/// chrome; the picker places its real children from its resolved layout.
struct EmptyContent;

impl Component for EmptyContent {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new("picker.surface.content"),
            constraints.constrain(LogicalSize::new(0, 0)),
        )
    }

    fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}
}

impl Component for PickerFrameComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        format!("{:?}", self.frame.policy).hash(&mut layout);
        self.frame.header.is_some().hash(&mut layout);
        self.frame.footer.is_some().hash(&mut layout);
        self.input.is_some().hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.frame.title.hash(&mut paint);
        format!("{:?}", self.frame.header).hash(&mut paint);
        format!("{:?}", self.frame.footer).hash(&mut paint);
        self.frame.styles.hash(&mut paint);
        let own = ComponentRevision::new(layout.finish(), paint.finish());
        combine_child_revisions(
            own,
            self.input
                .iter()
                .map(Element::revision)
                .chain(std::iter::once(self.list.revision())),
        )
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let outer = constraints.constrain(LogicalSize::new(
            constraints.max_width(),
            constraints
                .max_height()
                .unwrap_or_else(|| constraints.min_height()),
        ));
        let panel_size = self.panel_size(constraints.inset(
            self.frame.policy.margin.horizontal(),
            usize::from(self.frame.policy.margin.vertical()),
        ));
        let local = self.local_layout(panel_size);
        let mut children = Vec::with_capacity(2);
        if let (Some(input), Some(area)) = (&self.input, local.input) {
            let node = input.layout(Constraints::new(area.width, area.width, 1, Some(1)), cx);
            children.push(ChildLayout::new(area.x, usize::from(area.y), node));
        }
        let list = self.list.layout(
            Constraints::new(
                local.list.width,
                local.list.width,
                usize::from(local.list.height),
                Some(usize::from(local.list.height)),
            ),
            cx,
        );
        children.push(ChildLayout::new(
            local.list.x,
            usize::from(local.list.y),
            list,
        ));
        let panel = LayoutNode::with_children(
            LayoutId::new(format!("{}.panel", self.id.as_str())),
            panel_size,
            children,
        );
        let (x, y) = self.panel_origin(outer, panel_size);
        LayoutNode::with_children(self.id.clone(), outer, vec![ChildLayout::new(x, y, panel)])
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.paint_chrome(layout, cx);
        let Some(panel) = layout.children.first() else {
            return;
        };
        let mut components = self.input.iter().chain(std::iter::once(&self.list));
        for child in &panel.node.children {
            let Some(component) = components.next() else {
                break;
            };
            cx.with_child(
                i32::from(panel.x.saturating_add(child.x)),
                i64::try_from(panel.y.saturating_add(child.y)).unwrap_or(i64::MAX),
                LocalRect::new(
                    0,
                    0,
                    child.node.size.width,
                    u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
                ),
                |cx| component.paint(&child.node, cx),
            );
        }
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(panel) = layout.children.first() else {
            return EventOutcome::Ignored;
        };
        let mut components = self.input.iter().chain(std::iter::once(&self.list));
        for child in panel.node.children.iter().rev() {
            let Some(component) = components.next_back() else {
                break;
            };
            let x = panel.x.saturating_add(child.x);
            let y = panel.y.saturating_add(child.y);
            let outcome = cx.with_transform(
                x,
                y,
                i32::from(x),
                i64::try_from(y).unwrap_or(i64::MAX),
                Rect::new(
                    x,
                    u16::try_from(y).unwrap_or(u16::MAX),
                    child.node.size.width,
                    u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
                ),
                |cx| component.event(event, &child.node, cx),
            );
            if outcome != EventOutcome::Ignored {
                return outcome;
            }
        }
        EventOutcome::Ignored
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
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::composition::TextBlock;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Insets, Point, Rect, Size};
    use bmux_tui::paint::PaintCx;

    use super::{PickerFrame, PickerFrameComponent, PickerFramePlacement, PickerFramePolicy};

    fn resolved_layout(frame: &PickerFrame<'_>, area: Rect) -> super::PickerFrameLayout {
        frame.resolved_layout(area)
    }

    #[test]
    fn computes_full_palette_layout() {
        let frame = PickerFrame::new()
            .title("Commands")
            .header("Type to filter")
            .footer("enter select")
            .policy(PickerFramePolicy::palette().max_size(Size::new(40, 10)));

        let layout = resolved_layout(&frame, Rect::new(0, 0, 80, 24));

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

        let layout = resolved_layout(&frame, Rect::new(0, 0, 40, 10));

        assert_eq!(layout.input, None);
        assert_eq!(layout.footer, None);
        assert_eq!(layout.list.height, 0);
    }

    #[test]
    fn bare_layout_uses_whole_area_without_chrome() {
        let frame = PickerFrame::new().policy(PickerFramePolicy::bare());

        let layout = resolved_layout(&frame, Rect::new(1, 2, 12, 4));

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

        let layout = resolved_layout(&frame, Rect::new(0, 0, 40, 10));

        assert_eq!(layout.panel, Rect::new(30, 5, 10, 5));
    }

    #[test]
    fn tiny_area_degrades_without_invalid_rects() {
        let frame = PickerFrame::new().header("H").footer("F");

        let layout = resolved_layout(&frame, Rect::new(0, 0, 4, 3));

        assert!(layout.panel.width <= 4);
        assert!(layout.panel.height <= 3);
        assert!(layout.inner.width <= layout.panel.width);
        assert!(layout.inner.height <= layout.panel.height);
    }

    #[test]
    fn component_owns_picker_geometry_and_child_placement() {
        let picker = PickerFrame::new()
            .title("Pick")
            .header("Header")
            .footer("Footer")
            .policy(PickerFramePolicy {
                margin: Insets::all(0),
                min_size: Size::new(20, 8),
                max_size: Size::new(20, 8),
                placement: PickerFramePlacement::Anchored(Point::new(3, 2)),
                ..PickerFramePolicy::palette()
            });
        let component = PickerFrameComponent::new(
            "picker",
            picker,
            TextBlock::new("first\nsecond").id("picker.list"),
        )
        .input(TextBlock::new("query").id("picker.input"));

        let layout = component.layout(Constraints::new(40, 40, 12, Some(12)), &mut LayoutCx::new());
        let panel = &layout.children[0];
        assert_eq!((panel.x, panel.y), (3, 2));
        assert_eq!(panel.node.size.width, 20);
        assert_eq!(panel.node.size.height, 8);
        assert_eq!(panel.node.children.len(), 2);
        assert_eq!(panel.node.children[0].node.id.as_str(), "picker.input");
        assert_eq!(panel.node.children[1].node.id.as_str(), "picker.list");

        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 12));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert!(
            frame
                .buffer()
                .row_symbols(2)
                .is_some_and(|row| row.contains("Pick"))
        );
        assert!(
            frame
                .buffer()
                .row_symbols(4)
                .is_some_and(|row| row.contains("Header"))
        );
        assert!(
            frame
                .buffer()
                .row_symbols(6)
                .is_some_and(|row| row.contains("query"))
        );
    }

    #[test]
    fn component_without_input_places_only_the_list_child() {
        let component = PickerFrameComponent::new(
            "picker",
            PickerFrame::new().policy(PickerFramePolicy {
                margin: Insets::all(0),
                min_size: Size::new(12, 5),
                max_size: Size::new(12, 5),
                ..PickerFramePolicy::palette()
            }),
            TextBlock::new("item").id("picker.list"),
        );
        let layout = component.layout(Constraints::new(12, 12, 5, Some(5)), &mut LayoutCx::new());

        assert_eq!(layout.children[0].node.children.len(), 1);
        assert_eq!(
            layout.children[0].node.children[0].node.id.as_str(),
            "picker.list"
        );
    }
}
