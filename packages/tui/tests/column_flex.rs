use bmux_tui::{
    component::{Component, Constraints, LayoutCx},
    composition::{Column, Flex, TextBlock},
    geometry::Size,
};

#[test]
fn column_allocates_remaining_height_by_weight() {
    let column = Column::new()
        .child(TextBlock::new("header"))
        .flex(Flex::new(3, TextBlock::new("a")))
        .flex(Flex::new(1, TextBlock::new("b")))
        .child(TextBlock::new("footer"));
    let layout = column.layout(Constraints::tight(Size::new(40, 22)), &mut LayoutCx::new());
    assert_eq!(
        layout
            .children
            .iter()
            .map(|child| child.node.size.height)
            .collect::<Vec<_>>(),
        [1, 15, 5, 1]
    );
    assert_eq!(layout.children.last().unwrap().y, 21);
}

#[test]
fn unbounded_flex_preserves_intrinsic_height() {
    let column = Column::new()
        .flex(Flex::new(3, TextBlock::new("one")))
        .flex(Flex::new(1, TextBlock::new("two")));
    assert_eq!(
        column
            .layout(Constraints::for_width(40), &mut LayoutCx::new())
            .size
            .height,
        2
    );
}
