//! Audience viewer and canonical overflow controller. Navigation remains host-owned.
use crate::component::{HiddenContent, SlideComponent};
use crate::custom::{CustomRegistry, RichContent};
use crate::model::Deck;
use crate::navigation::PresentationState;
use crate::theme::PresentationTheme;
use bmux_tui::component::{ChildLayout, LogicalRect};
use bmux_tui::prelude::{
    Column, Component, ComponentRevision, Constraints, Element, Event, EventCx, EventOutcome, Fill,
    Flex, LayoutCx, LayoutId, LayoutNode, LogicalSize, PaintCx, Rect, TextBlock,
};
use bmux_tui_components::scroll_view::{
    ScrollView, ScrollViewComponent, ScrollViewOutcome, ScrollViewState,
};
use std::cell::Cell;

#[derive(Default)]
pub struct ViewerState {
    pub scroll: Cell<ScrollViewState>,
}
impl ViewerState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn reset_scroll(&self) {
        self.scroll.set(ScrollViewState::new());
    }
}

pub struct ViewerComponent<'a> {
    deck: &'a Deck,
    navigation: &'a PresentationState,
    state: &'a ViewerState,
    theme: &'a PresentationTheme,
    custom: &'a CustomRegistry<'a>,
    rich: Option<&'a dyn RichContent>,
    hidden: HiddenContent,
    focused_block: Option<&'a crate::model::BlockId>,
}
impl<'a> ViewerComponent<'a> {
    pub fn new(
        deck: &'a Deck,
        navigation: &'a PresentationState,
        state: &'a ViewerState,
        theme: &'a PresentationTheme,
        custom: &'a CustomRegistry<'a>,
    ) -> Self {
        Self {
            deck,
            navigation,
            state,
            theme,
            custom,
            rich: None,
            hidden: HiddenContent::ReserveSpace,
            focused_block: None,
        }
    }
    pub fn focused_block(mut self, id: &'a crate::model::BlockId) -> Self {
        self.focused_block = Some(id);
        self
    }
    pub fn rich_content(mut self, rich: &'a dyn RichContent) -> Self {
        self.rich = Some(rich);
        self
    }
    pub fn hidden_content(mut self, hidden: HiddenContent) -> Self {
        self.hidden = hidden;
        self
    }
    fn tree(&self) -> Element<'_> {
        let id = format!("presentation/{}/viewer", self.deck.id.as_str());
        let content = if let Some(slide) = self.navigation.current_slide(self.deck) {
            let mut component =
                SlideComponent::new(slide, self.navigation.reveal_step, self.theme, self.custom)
                    .hidden_content(self.hidden);
            if let Some(id) = self.focused_block {
                component = component.focused_block(id);
            }
            if let Some(rich) = self.rich {
                component = component.rich_content(rich);
            }
            Element::new(component)
        } else {
            Element::new(
                TextBlock::new("No slide selected")
                    .id(format!("{id}/empty"))
                    .style(self.theme.diagnostic),
            )
        };
        let slide_index = self
            .navigation
            .slide_id
            .as_ref()
            .and_then(|id| self.deck.slide_index(id));
        let footer = format!(
            "{}  {}/{}  reveal {}/{}",
            self.deck.metadata.title,
            slide_index.map_or(0, |index| index + 1),
            self.deck.slides.len(),
            self.navigation.reveal_step,
            self.navigation
                .current_slide(self.deck)
                .map_or(0, crate::model::Slide::max_reveal_step)
        );
        Element::new(
            Fill::new(
                self.theme.background,
                Column::new()
                    .id(id.clone())
                    .flex(Flex::new(
                        1,
                        ViewerViewport {
                            id: format!("{id}/viewport"),
                            state: self.state,
                            child: content,
                        },
                    ))
                    .child(
                        TextBlock::new(footer)
                            .id(format!("{id}/footer"))
                            .style(self.theme.footer),
                    ),
            )
            .id(format!("{id}/background")),
        )
    }
}
impl Component for ViewerComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        self.tree()
            .revision()
            .combine(self.custom.revision())
            .combine(
                self.rich
                    .map_or(ComponentRevision::default(), |rich| rich.revision()),
            )
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.tree().layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.tree().paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.tree().event(event, layout, cx)
    }
}

struct BorrowedComponent<'a>(&'a dyn Component);
impl Component for BorrowedComponent<'_> {
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

struct ViewerViewport<'a> {
    id: String,
    state: &'a ViewerState,
    child: Element<'a>,
}
impl ViewerViewport<'_> {
    fn viewport(&self, size: LogicalSize) -> ScrollViewComponent<'_> {
        ScrollViewComponent::new(
            format!("{}/content", self.id),
            size,
            self.state.scroll.get(),
            BorrowedComponent(&self.child),
        )
        .retain_state(&self.state.scroll)
    }
    fn area(size: LogicalSize) -> Rect {
        Rect::new(
            0,
            0,
            u16::try_from(size.width).unwrap_or(u16::MAX),
            u16::try_from(size.height).unwrap_or(u16::MAX),
        )
    }
}
impl Component for ViewerViewport<'_> {
    fn revision(&self) -> ComponentRevision {
        self.child.revision().combine(ComponentRevision::new(
            0,
            self.state
                .scroll
                .get()
                .vertical_offset()
                .wrapping_mul(31)
                .wrapping_add(self.state.scroll.get().horizontal_offset()),
        ))
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        let width = constraints.max_width();
        let height = constraints.max_height().unwrap_or_else(|| {
            self.child
                .layout(Constraints::for_width(width), cx)
                .size
                .height
        });
        let size = constraints.constrain(LogicalSize::new(width, height));
        let content = ScrollView::new().content_area(Self::area(size));
        let viewport_size = LogicalSize::new(u64::from(content.width), u64::from(content.height));
        let viewport = self.viewport(viewport_size).layout(
            Constraints::new(
                viewport_size.width,
                viewport_size.width,
                viewport_size.height,
                Some(viewport_size.height),
            ),
            cx,
        );
        LayoutNode::with_children(
            LayoutId::new(self.id.clone()),
            size,
            vec![ChildLayout::new(
                u64::from(content.x),
                u64::from(content.y),
                viewport,
            )],
        )
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if let Some(child) = layout.children.first() {
            cx.with_child_size(
                i32::try_from(child.x).unwrap_or(i32::MAX),
                i64::try_from(child.y).unwrap_or(i64::MAX),
                child.node.size,
                |cx| self.viewport(child.node.size).paint(&child.node, cx),
            );
            let controller = ScrollView::new();
            let mut state = self.state.scroll.get();
            controller.reconcile(&child.node, &mut state);
            self.state.scroll.set(state);
            controller.paint_chrome(
                self.id.clone(),
                Self::area(layout.size),
                &child.node,
                &state,
                cx,
            );
        }
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(child) = layout.children.first() else {
            return EventOutcome::Ignored;
        };
        let outcome = cx.with_child(child, |cx| {
            self.viewport(child.node.size).event(event, &child.node, cx)
        });
        if outcome.is_handled() {
            return outcome;
        }
        let area = cx.visible_rect(LogicalRect::new(
            0,
            0,
            layout.size.width,
            layout.size.height,
        ));
        let mut state = self.state.scroll.get();
        let outcome = ScrollView::new().handle_event(area, &child.node, &mut state, event);
        self.state.scroll.set(state);
        if outcome == ScrollViewOutcome::Ignored {
            EventOutcome::Ignored
        } else {
            EventOutcome::Redraw
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{DeckBuilder, SlideBuilder};
    use bmux_tui::prelude::{Buffer, Frame, Size};
    #[test]
    fn overflow_is_preserved_in_authoritative_scroll_tree() {
        let content = (0..50)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let deck = DeckBuilder::new("d")
            .slide(
                SlideBuilder::new("s")
                    .text("body", content)
                    .notes("SECRET")
                    .build(),
            )
            .build()
            .unwrap();
        let navigation = PresentationState::new(&deck);
        let state = ViewerState::new();
        let theme = PresentationTheme::dark();
        let custom = CustomRegistry::new();
        let viewer = ViewerComponent::new(&deck, &navigation, &state, &theme, &custom);
        let layout = viewer.layout(
            Constraints::tight(Size::new(30, 8)),
            &mut LayoutCx::default(),
        );
        let viewport = layout
            .find(&LayoutId::new("presentation/d/viewer/viewport/content"))
            .unwrap();
        assert!(ScrollView::max_vertical_offset(viewport) > 40);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 8));
        let mut frame = Frame::new(&mut buffer);
        viewer.paint(&layout, &mut PaintCx::new(&mut frame));
        let visible = (0..8)
            .filter_map(|row| frame.buffer().row_symbols(row))
            .collect::<String>();
        assert!(!visible.contains("SECRET"));
        assert!(!frame.hits().regions().is_empty());
        let mut scrolled = state.scroll.get();
        scrolled.set_vertical_offset(40);
        state.scroll.set(scrolled);
        viewer.paint(&layout, &mut PaintCx::new(&mut frame));
        assert!(state.scroll.get().vertical_offset() > 0);
    }
    #[test]
    fn empty_deck_has_a_measurable_viewer() {
        let deck = DeckBuilder::new("d").build().unwrap();
        let navigation = PresentationState::new(&deck);
        let state = ViewerState::new();
        let theme = PresentationTheme::dark();
        let custom = CustomRegistry::new();
        let viewer = ViewerComponent::new(&deck, &navigation, &state, &theme, &custom);
        let layout = viewer.layout(
            Constraints::tight(Size::new(1, 1)),
            &mut LayoutCx::default(),
        );
        assert_eq!(layout.size, LogicalSize::new(1, 1));
    }
}
