use bmux_markdown::{
    MarkdownContributionKind, MarkdownRenderOptions, MarkdownSemanticEventKind,
    MarkdownSemanticTag, MermaidContributionRendering, MermaidRenderMode, parse_markdown_document,
    render_markdown,
};
use bmux_tui::prelude::Line;

#[test]
fn extracted_renderer_preserves_canonical_source_and_tui_identity() {
    let source = "# 東京 🧪\n\n**é** and [label](https://example.com).\n\n![plot](plot.png)\n";
    let document = parse_markdown_document(source);
    for event in &document.events {
        assert!(source.get(event.source_range.clone()).is_some());
    }
    assert!(document.events.iter().any(|event| matches!(
        &event.kind,
        MarkdownSemanticEventKind::Start(MarkdownSemanticTag::Image(image))
            if image.source == "plot.png"
    )));

    let options = MarkdownRenderOptions::new(20);
    let rendered = render_markdown(source, &options);
    let _: &[Line] = &rendered.lines;
    assert!(rendered.has_selection_source(source));
    assert!(!rendered.has_selection_source("different source"));
    for unit in rendered.selection_provenance() {
        for range in &unit.source_ranges {
            assert!(source.get(range.clone()).is_some());
        }
    }
    assert!(rendered.contributions.iter().any(|contribution| matches!(
        &contribution.kind,
        MarkdownContributionKind::Image { image, alt, .. }
            if image.source == "plot.png" && alt == "plot"
    )));
}

#[test]
fn mermaid_contribution_preserves_source_without_in_process_execution() {
    let source = "```mermaid\nflowchart LR\nA --> B\n```\n";
    let mut options = MarkdownRenderOptions::new(40);
    options.mermaid = MermaidRenderMode::Contribution;
    let rendered = render_markdown(source, &options);
    assert!(rendered.contributions.iter().any(|contribution| matches!(
        &contribution.kind,
        MarkdownContributionKind::Mermaid { source, rendering, .. }
            if source.contains("A --> B")
                && matches!(rendering, MermaidContributionRendering::Disabled)
    )));
    assert!(rendered.has_selection_source(source));
}
