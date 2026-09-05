use bmux_tui::{
    component::{Component, Constraints, LayoutCx},
    composition::TextBlock,
    text::Text,
};

#[test]
fn plain_text_constructors_split_lines_consistently() {
    for (input, expected) in [
        ("", vec![""]),
        ("one", vec!["one"]),
        ("one\ntwo", vec!["one", "two"]),
        ("one\r\ntwo", vec!["one", "two"]),
        ("\n\none\n", vec!["", "", "one", ""]),
        ("one\r\n\r\n", vec!["one", "", ""]),
    ] {
        for text in [
            Text::raw(input),
            Text::from(input),
            Text::from(input.to_owned()),
        ] {
            assert_eq!(
                text.lines
                    .iter()
                    .map(bmux_tui::text::Line::plain_text)
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
}

#[test]
fn multiline_text_block_measures_each_line() {
    let block = TextBlock::new("Repository: example\nInstructions\n\nChoose tools");
    let layout = block.layout(Constraints::for_width(80), &mut LayoutCx::new());
    assert_eq!(layout.size.height, 4);
}
