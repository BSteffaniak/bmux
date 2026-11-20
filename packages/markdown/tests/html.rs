//! Raw HTML presentation behavior.

#![cfg(feature = "html")]

use std::collections::BTreeMap;

use bmux_markdown::{
    MarkdownColorScheme, MarkdownContributionKind, MarkdownDestination, MarkdownDocumentContext,
    MarkdownHtmlMode, MarkdownLinkKind, MarkdownRenderOptions, MarkdownRenderResult,
    MarkdownSelectionKind, render_markdown,
};
use bmux_tui::prelude::{Line, Modifier};

const PR_TEMPLATE: &str = "\
<!-- Thanks for sending a pull request! Please fill out the sections below. -->
## Summary

<!-- What does this change and why? -->
Renders **embedded HTML** in descriptions.

<p align=\"center\">
  <img src=\"https://example.com/demo.png\" width=\"320\" alt=\"Demo\">
</p>

## Checklist
<!--
- [ ] Remove this hidden reminder
-->
- [x] Tests added
- [ ] Docs updated

<details>
<summary>Screenshots</summary>

**Before** and *after* the change:

- first
- second

</details>

Press <kbd>Ctrl</kbd>+<kbd>C</kbd> to copy.

<table>
  <tr><th>Case</th><th align=\"right\">Result</th></tr>
  <tr><td>render</td><td align=\"right\">ok</td></tr>
</table>
";

/// Every allowed element name; rendered output never shows one as a tag.
const ALLOWED: &[&str] = &[
    "a",
    "abbr",
    "b",
    "blockquote",
    "br",
    "caption",
    "cite",
    "code",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "picture",
    "pre",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "small",
    "source",
    "span",
    "strike",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "wbr",
];

fn options(width: u16) -> MarkdownRenderOptions {
    MarkdownRenderOptions::new(width)
}

fn rows(result: &MarkdownRenderResult) -> Vec<String> {
    result.lines.iter().map(line_text).collect()
}

fn line_text(line: &Line) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn text(markdown: &str, width: u16) -> String {
    rows(&render_markdown(markdown, &options(width))).join("\n")
}

fn assert_no_allowed_tags(rendered: &str) {
    for name in ALLOWED {
        for tag in [
            format!("<{name}>"),
            format!("<{name} "),
            format!("</{name}>"),
        ] {
            assert!(
                !rendered.contains(&tag),
                "rendered output shows `{tag}`:\n{rendered}"
            );
        }
    }
}

fn contribution_id(result: &MarkdownRenderResult, prefix: &str) -> String {
    result
        .contributions
        .iter()
        .find(|item| item.id.starts_with(prefix))
        .map(|item| item.id.clone())
        .unwrap_or_else(|| panic!("no `{prefix}` contribution in {:?}", result.contributions))
}

fn geometry_text(result: &MarkdownRenderResult, id: &str) -> String {
    let geometry = result
        .geometry
        .iter()
        .find(|item| item.contribution_id == id)
        .unwrap_or_else(|| panic!("no geometry for {id}"));
    geometry
        .rects
        .iter()
        .map(|rect| {
            result.lines[usize::from(rect.y)]
                .viewport(usize::from(rect.x), usize::from(rect.width))
                .plain_text()
        })
        .collect::<Vec<_>>()
        .join("|")
}

#[test]
fn pull_request_template_hides_comments_and_renders_elements() {
    let rendered = text(PR_TEMPLATE, 60);
    assert!(!rendered.contains("<!--"), "{rendered}");
    assert!(!rendered.contains("Thanks for sending"), "{rendered}");
    assert!(!rendered.contains("hidden reminder"), "{rendered}");
    assert!(rendered.contains("Renders embedded HTML in descriptions."));
    assert!(rendered.contains("[image: Demo]"));
    assert!(rendered.contains("Screenshots"));
    assert!(rendered.contains("Before and after the change:"));
    assert!(rendered.contains("•  first"));
    assert!(rendered.contains("Press Ctrl+C to copy."));
    assert!(rendered.contains("│ Case   │ Result │"), "{rendered}");
    assert!(rendered.contains("│ render │     ok │"), "{rendered}");
    assert_no_allowed_tags(&rendered);
}

#[test]
fn details_body_written_in_markdown_follows_disclosure_state() {
    let markdown =
        "<details>\n<summary>Screenshots</summary>\n\n**Before**\n\n- one\n\n</details>\n\nAfter.";
    assert_eq!(
        text(markdown, 40),
        "Screenshots\n\nBefore\n\n•  one\n\nAfter."
    );

    let closed = render_markdown(markdown, &options(40).with_details_open(BTreeMap::new()));
    assert_eq!(rows(&closed), ["▶ Screenshots", "", "After."]);
    let id = contribution_id(&closed, "details:");
    assert_eq!(
        id,
        format!("details:0:{}", markdown.find("\n\nAfter").unwrap())
    );
    assert_eq!(geometry_text(&closed, &id), "▶ Screenshots");

    let open = render_markdown(
        markdown,
        &options(40).with_details_open(BTreeMap::from([(id.clone(), true)])),
    );
    assert_eq!(
        rows(&open),
        ["▼ Screenshots", "", "Before", "", "•  one", "", "After."]
    );
    assert_eq!(geometry_text(&open, &id), "▼ Screenshots");
}

#[test]
fn details_identities_match_the_raw_projection() {
    let markdown = "<details open>\n<summary>Outer</summary>\n\n<details>\n<summary>Inner</summary>\n\nBody\n\n</details>\n\n</details>";
    let ids = |mode| {
        let result = render_markdown(
            markdown,
            &options(40)
                .with_html(mode)
                .with_document_id("doc")
                .with_details_open(BTreeMap::new()),
        );
        result
            .contributions
            .iter()
            .filter(|item| matches!(item.kind, MarkdownContributionKind::Details { .. }))
            .map(|item| item.id.clone())
            .collect::<Vec<_>>()
    };
    let rendered = ids(MarkdownHtmlMode::Render);
    assert_eq!(rendered.len(), 2);
    assert_eq!(rendered, ids(MarkdownHtmlMode::Raw));

    let result = render_markdown(
        markdown,
        &options(40)
            .with_document_id("doc")
            .with_details_open(BTreeMap::new()),
    );
    assert_eq!(rows(&result), ["▼ Outer", "", "▶ Inner"]);
    assert_eq!(geometry_text(&result, &rendered[0]), "▼ Outer");
    assert_eq!(geometry_text(&result, &rendered[1]), "▶ Inner");
}

#[test]
fn aligned_paragraph_centers_image_and_its_geometry() {
    let markdown = "<p align=\"center\">\n  <img src=\"https://example.com/logo.png\" width=\"120\" height=\"40\" alt=\"Logo\">\n</p>";
    let result = render_markdown(markdown, &options(40));
    let offset = (40 - "[image: Logo]".len()) / 2;
    assert_eq!(
        rows(&result),
        [format!("{}[image: Logo]", " ".repeat(offset))]
    );
    let image = result
        .contributions
        .iter()
        .find_map(|item| match &item.kind {
            MarkdownContributionKind::Image {
                image, alt, source, ..
            } => Some((item.id.clone(), image.clone(), alt.clone(), source.clone())),
            _ => None,
        })
        .expect("image contribution");
    assert_eq!(image.1.kind, MarkdownLinkKind::Html);
    assert_eq!((image.1.width, image.1.height), (Some(120), Some(40)));
    assert_eq!(image.2, "Logo");
    assert!(matches!(image.3, MarkdownDestination::Web(_)));
    let rect = result
        .geometry
        .iter()
        .find(|item| item.contribution_id == image.0)
        .unwrap()
        .rects[0];
    assert_eq!(usize::from(rect.x), offset);
    assert_eq!(geometry_text(&result, &image.0), "[image: Logo]");
}

#[test]
fn reserved_image_rows_keep_the_alignment_of_their_label() {
    let markdown = "<p align=\"right\"><img src=\"https://example.com/a.png\" alt=\"A\"></p>";
    let result = render_markdown(markdown, &options(20).with_rich_reserved_rows(3, 1));
    let id = contribution_id(&result, "image:");
    let rects = &result
        .geometry
        .iter()
        .find(|item| item.contribution_id == id)
        .unwrap()
        .rects;
    assert_eq!(rects.len(), 3);
    assert!(rects.iter().all(|rect| rect.x == rects[0].x), "{rects:?}");
    assert_eq!(usize::from(rects[0].x), 20 - "[image: A]".len());
}

#[test]
fn picture_uses_its_fallback_image() {
    let markdown = "<picture>\n  <source media=\"(prefers-color-scheme: dark)\" srcset=\"dark.png\">\n  <source media=\"(prefers-color-scheme: light)\" srcset=\"light.png\">\n  <img alt=\"Logo\" src=\"fallback.png\">\n</picture>";
    let result = render_markdown(markdown, &options(40));
    assert_eq!(rows(&result), ["[image: Logo]"]);
    let sources = result
        .contributions
        .iter()
        .filter_map(|item| match &item.kind {
            MarkdownContributionKind::Image { image, .. } => Some(image.source.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(sources, ["fallback.png"]);

    let sources_only =
        "<picture><source srcset=\"wide.png 2x, narrow.png\" media=\"(min-width: 9px)\"></picture>";
    assert_eq!(text(sources_only, 40), "[image: wide.png]");
}

fn image_sources(result: &MarkdownRenderResult) -> Vec<(String, String)> {
    result
        .contributions
        .iter()
        .filter_map(|item| match &item.kind {
            MarkdownContributionKind::Image { image, alt, .. } => {
                Some((image.source.clone(), alt.clone()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn picture_prefers_the_source_for_the_color_scheme() {
    let markdown = "<picture>\n  <source media=\"(prefers-color-scheme: dark)\" srcset=\"dark.png 2x, dark-1x.png\">\n  <source media=\"( Prefers-Color-Scheme : LIGHT )\" srcset=\"light.png\">\n  <img alt=\"Logo\" src=\"fallback.png\" width=\"40\">\n</picture>";
    let render = |scheme: Option<MarkdownColorScheme>| {
        let mut options = options(40);
        options.color_scheme = scheme;
        render_markdown(markdown, &options)
    };
    let unset = render(None);
    assert_eq!(
        image_sources(&unset),
        [("fallback.png".to_owned(), "Logo".to_owned())]
    );
    let dark = render(Some(MarkdownColorScheme::Dark));
    // The fallback image's alt text, dimensions, and identity stay; only the
    // source follows the scheme.
    assert_eq!(
        image_sources(&dark),
        [("dark.png".to_owned(), "Logo".to_owned())]
    );
    assert_eq!(rows(&dark), rows(&unset));
    assert_eq!(dark.contributions[0].id, unset.contributions[0].id);
    let light = render_markdown(
        markdown,
        &options(40).with_color_scheme(MarkdownColorScheme::Light),
    );
    assert_eq!(
        image_sources(&light),
        [("light.png".to_owned(), "Logo".to_owned())]
    );

    // Without a source for the scheme, the fallback image is kept.
    let dark_only = "<picture><source media=\"(prefers-color-scheme: dark)\" srcset=\"dark.png\"><img alt=\"Logo\" src=\"fallback.png\"></picture>";
    let light = render_markdown(
        dark_only,
        &options(40).with_color_scheme(MarkdownColorScheme::Light),
    );
    assert_eq!(
        image_sources(&light),
        [("fallback.png".to_owned(), "Logo".to_owned())]
    );
}

#[test]
fn picture_without_fallback_prefers_the_scheme_then_unconditional_sources() {
    let markdown = "<picture><source media=\"(min-width: 9px)\" srcset=\"wide.png\"><source srcset=\"any.png\"><source media=\"(prefers-color-scheme: dark)\" srcset=\"dark.png\"></picture>";
    let source = |scheme: Option<MarkdownColorScheme>| {
        let mut options = options(40);
        options.color_scheme = scheme;
        image_sources(&render_markdown(markdown, &options))
            .into_iter()
            .map(|(source, _)| source)
            .collect::<Vec<_>>()
    };
    assert_eq!(source(None), ["any.png"]);
    assert_eq!(source(Some(MarkdownColorScheme::Light)), ["any.png"]);
    assert_eq!(source(Some(MarkdownColorScheme::Dark)), ["dark.png"]);
}

#[test]
fn inline_semantics_map_to_terminal_styles() {
    let markdown = "Press <kbd>Ctrl</kbd>+<kbd>C</kbd>. H<sub>2</sub>O, x<sup>2</sup>, a<sup>bc</sup>, b<sub>x</sub>, <mark>hot</mark>, <ins>new</ins>, <del>old</del>, <q>quote</q>.";
    let result = render_markdown(markdown, &options(100));
    assert_eq!(
        rows(&result),
        ["Press Ctrl+C. H₂O, x², a^(bc), b_x, hot, new, old, “quote”."]
    );
    let theme = options(100).theme;
    let style_of = |needle: &str| {
        result.lines[0]
            .spans
            .iter()
            .find(|span| span.content == needle)
            .map(|span| span.style)
            .unwrap_or_else(|| panic!("no span `{needle}` in {:?}", result.lines[0].spans))
    };
    assert_eq!(style_of("Ctrl"), theme.text.patch(theme.keyboard));
    assert_eq!(style_of("hot"), theme.text.patch(theme.highlight));
    assert!(style_of("new").modifiers.contains(Modifier::UNDERLINE));
    assert!(style_of("old").modifiers.contains(Modifier::CROSSED_OUT));
}

#[test]
fn html_tables_support_headers_alignment_and_colspan() {
    let markdown = "<table>\n<thead><tr><th>Name</th><th align=\"right\">Value</th></tr></thead>\n<tbody>\n<tr><td colspan=\"2\">Spanning</td></tr>\n<tr><td>a</td><td align=\"right\">1</td></tr>\n</tbody>\n</table>";
    assert_eq!(
        text(markdown, 40),
        "┌──────────┬───────┐\n│ Name     │ Value │\n├──────────┼───────┤\n│ Spanning │       │\n├──────────┼───────┤\n│ a        │     1 │\n└──────────┴───────┘"
    );
}

#[test]
fn html_tables_lay_out_rowspan_as_empty_cells_below() {
    let markdown = "<table>\n<tr><th>Group</th><th>Item</th><th>Note</th></tr>\n<tr><td rowspan=\"2\">fruit</td><td>apple</td><td>red</td></tr>\n<tr><td>pear</td><td>green</td></tr>\n<tr><td>nut</td><td>almond</td><td>-</td></tr>\n</table>";
    assert_eq!(
        text(markdown, 40),
        "┌───────┬────────┬───────┐\n│ Group │ Item   │ Note  │\n├───────┼────────┼───────┤\n│ fruit │ apple  │ red   │\n├───────┼────────┼───────┤\n│       │ pear   │ green │\n├───────┼────────┼───────┤\n│ nut   │ almond │ -     │\n└───────┴────────┴───────┘"
    );
}

#[test]
fn html_rowspan_in_later_columns_and_with_colspan_keeps_columns_aligned() {
    // A span in the last column leaves the next row's cells in place; a cell
    // spanning rows and columns leaves empty cells across both.
    let markdown = "<table>\n<tr><th>A</th><th>B</th><th>C</th></tr>\n<tr><td>1</td><td>2</td><td rowspan=\"2\">tall</td></tr>\n<tr><td>3</td><td>4</td></tr>\n<tr><td colspan=\"2\" rowspan=\"2\">big</td><td>5</td></tr>\n<tr><td>6</td></tr>\n</table>";
    assert_eq!(
        text(markdown, 40),
        "┌─────┬───┬──────┐\n│ A   │ B │ C    │\n├─────┼───┼──────┤\n│ 1   │ 2 │ tall │\n├─────┼───┼──────┤\n│ 3   │ 4 │      │\n├─────┼───┼──────┤\n│ big │   │ 5    │\n├─────┼───┼──────┤\n│     │   │ 6    │\n└─────┴───┴──────┘"
    );
}

#[test]
fn html_rowspan_ends_with_its_row_group() {
    let markdown = "<table>\n<thead><tr><th rowspan=\"0\">H</th><th>I</th></tr></thead>\n<tbody><tr><td>a</td><td>b</td></tr></tbody>\n</table>";
    assert_eq!(
        text(markdown, 40),
        "┌───┬───┐\n│ H │ I │\n├───┼───┤\n│ a │ b │\n└───┴───┘"
    );
}

#[test]
fn markdown_table_alignment_is_not_shifted_by_html_tables() {
    let markdown = "<table><tr><td>x</td></tr></table>\n\n| a | b |\n|--:|:--|\n| 1 | 22 |";
    let rendered = text(markdown, 40);
    assert!(rendered.contains("│ a │ b  │"), "{rendered}");
    assert!(rendered.contains("│ 1 │ 22 │"), "{rendered}");
}

#[test]
fn nested_html_lists_and_markdown_blockquote_alerts_keep_structure() {
    assert_eq!(
        text(
            "<ul><li>one<ul><li>nested</li></ul></li><li>two</li></ul>",
            40
        ),
        "•  one\n   •  nested\n•  two"
    );
    assert_eq!(
        text("<ol><li>first<li>second</ol>", 40),
        "1.  first\n2.  second"
    );
    let alert = text(
        "<blockquote>quoted</blockquote>\n\n> [!NOTE]\n> Alert body",
        40,
    );
    assert!(alert.contains("│ quoted"), "{alert}");
    assert!(alert.contains("ⓘ NOTE"), "{alert}");
}

#[test]
fn links_resolve_safely_against_document_context() {
    let markdown = "<a href=\"docs/guide.md\">Guide</a> and <a href=\"javascript:alert(1)\">bad</a> and <a name=\"anchor\">plain</a>";
    let context = MarkdownDocumentContext {
        base_url: Some(url::Url::parse("https://example.com/repo/").unwrap()),
        ..MarkdownDocumentContext::default()
    };
    let result = render_markdown(markdown, &options(80).with_document_context(context));
    assert_eq!(rows(&result), ["Guide and bad and plain"]);
    let links = result
        .contributions
        .iter()
        .filter_map(|item| match &item.kind {
            MarkdownContributionKind::Link {
                destination,
                label,
                link,
            } => Some((
                item.id.clone(),
                label.clone(),
                destination.clone(),
                link.kind,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(links.len(), 2);
    assert_eq!(links[0].1, "Guide");
    assert_eq!(
        links[0].2,
        MarkdownDestination::Web(
            url::Url::parse("https://example.com/repo/docs/guide.md").unwrap()
        )
    );
    assert_eq!(links[0].3, MarkdownLinkKind::Html);
    assert!(matches!(links[1].2, MarkdownDestination::Inert { .. }));
    assert_eq!(geometry_text(&result, &links[0].0), "Guide");
    assert_eq!(geometry_text(&result, &links[1].0), "bad");
}

#[test]
fn readable_fallbacks_follow_html_links_and_keep_selection_aligned() {
    let markdown = "<a href=\"https://example.com/x\">site</a> tail";
    let result = render_markdown(markdown, &options(80).with_link_destination_fallbacks(true));
    assert_eq!(rows(&result), ["site (https://example.com/x) tail"]);
    let tail = markdown.find("tail").unwrap();
    let unit = result
        .selection_provenance()
        .iter()
        .find(|unit| unit.source_ranges[0].start == tail)
        .expect("tail provenance");
    assert_eq!(
        usize::from(unit.rects[0].x),
        "site (https://example.com/x) ".len()
    );
}

#[test]
fn markdown_and_html_links_keep_their_own_geometry() {
    let markdown =
        "<a href=\"https://a.example\">html</a> then [md](https://b.example) and ![pic](p.png)";
    let result = render_markdown(markdown, &options(80));
    for item in &result.contributions {
        let shown = geometry_text(&result, &item.id);
        let expected = match &item.kind {
            MarkdownContributionKind::Link { label, .. } => label.clone(),
            MarkdownContributionKind::Image { alt, .. } => format!("[image: {alt}]"),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(shown, expected);
    }
}

#[test]
fn script_and_style_contents_are_removed() {
    let markdown = "<script>alert(\"</p>\")</script>\n\n<style>p { color: red }</style>\n\nVisible <iframe src=\"x\">frame text</iframe>end";
    assert_eq!(text(markdown, 40), "Visible end");
}

#[test]
fn entities_are_decoded() {
    assert_eq!(
        text(
            "<p>Fish &amp; chips &mdash; &copy; &#x1F600; &#65; &bogus;</p>",
            60
        ),
        "Fish & chips — © 😀 A &bogus;"
    );
}

#[test]
fn malformed_markup_degrades_to_text() {
    let markdown = "<div><b>bold <i>both</div> after\n\n</span> stray <b>never closed\n\na < b > c <notatag and <3";
    let rendered = text(markdown, 80);
    assert_eq!(
        rendered,
        "bold both\nafter\n\nstray never closed\n\na < b > c <notatag and <3"
    );
}

#[test]
fn inline_html_inside_markdown_paragraphs() {
    let markdown = "Use <b>bold</b>, <em>em</em>, <code>code</code><br>\nnext line";
    let result = render_markdown(markdown, &options(80));
    assert_eq!(rows(&result), ["Use bold, em, code", "next line"]);
    let theme = options(80).theme;
    let span = |needle: &str| {
        result.lines[0]
            .spans
            .iter()
            .find(|span| span.content == needle)
            .map(|span| span.style)
            .unwrap()
    };
    assert_eq!(span("bold"), theme.text.patch(theme.strong));
    assert_eq!(span("code"), theme.text.patch(theme.inline_code));
}

#[test]
fn headings_preformatted_text_and_definition_lists() {
    let markdown = "<h2 align=\"right\">Title</h2>\n\n<pre>\n  keep   spacing\n</pre>\n\n<dl><dt>Term</dt><dd>Definition</dd></dl>\n\n# Markdown heading";
    let result = render_markdown(markdown, &options(20));
    assert_eq!(
        rows(&result),
        [
            "               Title",
            "",
            "  keep   spacing",
            "",
            "Term",
            "  Definition",
            "",
            "Markdown heading"
        ]
    );
    // Only Markdown headings are document anchors.
    assert_eq!(result.anchors.len(), 1);
    assert_eq!(result.anchors[0].fragment, "markdown-heading");
}

#[test]
fn strip_mode_removes_tags_and_keeps_text() {
    let result = render_markdown(PR_TEMPLATE, &options(60).with_html(MarkdownHtmlMode::Strip));
    let rendered = rows(&result).join("\n");
    assert!(!rendered.contains("hidden reminder"));
    assert!(rendered.contains("Demo"));
    assert!(!rendered.contains("[image: Demo]"));
    assert!(rendered.contains("Screenshots"));
    assert!(rendered.contains("Press Ctrl+C to copy."));
    assert!(rendered.contains("Case Result"), "{rendered}");
    assert_no_allowed_tags(&rendered);
    assert!(
        result
            .contributions
            .iter()
            .all(|item| !matches!(item.kind, MarkdownContributionKind::Details { .. })),
    );
    let theme = options(60).theme;
    assert!(
        result
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style != theme.text.patch(theme.keyboard))
    );
}

#[test]
fn raw_mode_shows_html_source() {
    let result = render_markdown(
        "Use <b>bold</b> <!-- note -->",
        &options(80).with_html(MarkdownHtmlMode::Raw),
    );
    assert_eq!(rows(&result), ["Use <b>bold</b> <!-- note -->"]);
}

#[test]
fn allowed_tags_never_render_literally_across_the_corpus() {
    let corpus = [
        PR_TEMPLATE,
        "<details open><summary><b>Bold</b> summary</summary>\n<p>Body <a href=\"x\">link</a></p>\n</details>",
        "<div align=\"center\">\n\n# Title\n\n<img src=\"a.png\"> <img src=\"b.png\" alt=\"B\">\n\n</div>",
        "<ruby>漢<rp>(</rp><rt>kan</rt><rp>)</rp></ruby> <abbr title=\"x\">HTML</abbr> <small>s</small> <span>t</span> <tt>u</tt> <samp>v</samp> <var>w</var> <cite>c</cite> <dfn>d</dfn> <s>e</s> <strike>f</strike> a<wbr>b",
        "<figure><img src=\"f.png\" alt=\"F\"><figcaption>Caption</figcaption></figure>\n\n<table><caption>Cap</caption><tfoot><tr><td>foot</td></tr></tfoot></table>\n\n<hr>\n\n<h1>One</h1><h3>Three</h3><h6>Six</h6>",
    ];
    for markdown in corpus {
        for width in [1, 3, 12, 80] {
            for mode in [MarkdownHtmlMode::Render, MarkdownHtmlMode::Strip] {
                let rendered =
                    rows(&render_markdown(markdown, &options(width).with_html(mode))).join("\n");
                assert_no_allowed_tags(&rendered);
            }
        }
    }
}

#[test]
fn arbitrary_markup_never_panics_and_stays_within_width() {
    const PIECES: &[&str] = &[
        "<details>",
        "</details>",
        "<summary>",
        "</summary>",
        "<b>",
        "</b>",
        "<p align=center>",
        "</p>",
        "<img src=x alt=\"漢字\">",
        "<br>",
        "<!--",
        "-->",
        "<table>",
        "<tr>",
        "<td colspan=9>",
        "</table>",
        "<ul>",
        "<li>",
        "</ul>",
        "<pre>",
        "</pre>",
        "<script>",
        "</script>",
        "&amp;",
        "&#x1F600;",
        "&",
        "<",
        ">",
        "\"",
        "'",
        "\n",
        "\n\n",
        " ",
        "text",
        "é",
        "👩‍👩‍👧",
        "**",
        "`",
        "- ",
        "<a href=\"javascript:x\">",
        "</a>",
        "<sup>",
        "</sup>",
        "<picture>",
        "<source srcset=y>",
        "<kbd>",
        "<div>",
        "</div>",
        "<h1 align=right>",
        "</h1>",
        "<blockquote>",
        "<q>",
        "<dd>",
    ];
    let mut state = 0x2545_f491_u64;
    for _ in 0..300 {
        let mut markdown = String::new();
        let pieces = 1 + (state % 24) as usize;
        for _ in 0..pieces {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            markdown.push_str(PIECES[(state >> 33) as usize % PIECES.len()]);
        }
        for width in [1, 2, 5, 17, 60] {
            for mode in [MarkdownHtmlMode::Render, MarkdownHtmlMode::Strip] {
                let result = render_markdown(
                    &markdown,
                    &options(width)
                        .with_html(mode)
                        .with_details_open(BTreeMap::new())
                        .with_link_destination_fallbacks(true),
                );
                for line in &result.lines {
                    assert!(
                        line.width() <= usize::from(width),
                        "row wider than {width}: {:?} from {markdown:?}",
                        line_text(line)
                    );
                }
                let _ = result.selection_provenance();
            }
        }
    }
}

#[test]
fn tiny_widths_keep_rows_and_hit_regions_inside_the_viewport() {
    let markdown = "<p align=\"center\"><a href=\"https://example.com\">漢字リンク</a> 😀 e\u{301}</p>\n\n<details><summary>摘要 summary</summary>\n\nbody\n\n</details>";
    for width in 1..=8_u16 {
        let result = render_markdown(markdown, &options(width).with_details_open(BTreeMap::new()));
        for line in &result.lines {
            assert!(line.width() <= usize::from(width), "{:?}", line_text(line));
        }
        for item in &result.geometry {
            for rect in &item.rects {
                assert!(
                    rect.x + rect.width <= width,
                    "rect {rect:?} outside width {width} for {}",
                    item.contribution_id
                );
                let row = &result.lines[usize::from(rect.y)];
                assert!(usize::from(rect.x + rect.width) <= row.width());
            }
        }
    }
}

#[test]
fn centered_wide_graphemes_use_display_width() {
    let markdown = "<p align=\"center\">漢字 😀 e\u{301}</p>";
    let result = render_markdown(markdown, &options(20));
    let row = &result.lines[0];
    let content_width = bmux_tui::text_width::display_width("漢字 😀 e\u{301}");
    assert_eq!(content_width, 9);
    assert_eq!(row.width(), (20 - content_width) / 2 + content_width);
}

#[test]
fn html_text_selection_maps_to_source_and_hidden_text_claims_no_cells() {
    let markdown = "<p>Hello <b>world</b></p>\n\n<details>\n<summary>Sum</summary>\n\nSecret words\n\n</details>\n\nHello again";
    let result = render_markdown(markdown, &options(40).with_details_open(BTreeMap::new()));
    assert_eq!(
        rows(&result),
        ["Hello world", "", "▶ Sum", "", "Hello again"]
    );
    let cell = |offset: usize| {
        let unit = result
            .selection_provenance()
            .iter()
            .find(|unit| {
                unit.kind == MarkdownSelectionKind::Text
                    && unit
                        .source_ranges
                        .first()
                        .is_some_and(|range| range.start == offset)
            })
            .unwrap_or_else(|| panic!("no text provenance at {offset}"));
        unit.rects.first().map(|rect| (rect.x, rect.y))
    };
    let world = markdown.find("world").unwrap();
    assert_eq!(cell(world), Some(("Hello ".len() as u16, 0)));
    let summary = markdown.find("Sum<").unwrap();
    assert_eq!(cell(summary), Some(("▶ ".chars().count() as u16, 2)));
    let secret = markdown.find("Secret").unwrap();
    assert_eq!(cell(secret), None);
    let again = markdown.rfind("again").unwrap();
    assert_eq!(cell(again), Some(("Hello ".len() as u16, 4)));
}

#[test]
fn transformed_superscripts_claim_no_cells() {
    let markdown = "x<sup>2</sup> then 2";
    let result = render_markdown(markdown, &options(40));
    assert_eq!(rows(&result), ["x² then 2"]);
    let inner = markdown.find('2').unwrap();
    let unit = result
        .selection_provenance()
        .iter()
        .find(|unit| {
            unit.source_ranges[0].start == inner && unit.kind == MarkdownSelectionKind::Text
        })
        .unwrap();
    assert!(unit.rects.is_empty());
    let last = markdown.rfind('2').unwrap();
    let unit = result
        .selection_provenance()
        .iter()
        .find(|unit| unit.source_ranges[0].start == last)
        .unwrap();
    assert_eq!(unit.rects[0].x, 8);
}

#[test]
fn tags_split_across_source_lines_are_completed() {
    let markdown = "<p align=\"center\">\n  <img\n    src=\"https://example.com/a.png\"\n    alt=\"Logo\">\n</p>\n\n> <img\n> src=\"b.png\" alt=\"Quoted\">";
    let result = render_markdown(markdown, &options(40));
    let rendered = rows(&result).join("\n");
    assert!(rendered.contains("[image: Logo]"), "{rendered}");
    assert!(rendered.contains("[image: Quoted]"), "{rendered}");
    assert!(!rendered.contains("<img"), "{rendered}");
    let ranges = result
        .contributions
        .iter()
        .filter(|item| matches!(item.kind, MarkdownContributionKind::Image { .. }))
        .map(|item| item.source_range.clone())
        .collect::<Vec<_>>();
    let first = markdown.find("<img").unwrap();
    let first_end = markdown.find("\"Logo\">").unwrap() + "\"Logo\">".len();
    let second = markdown.rfind("<img").unwrap();
    assert_eq!(ranges, [first..first_end, second..markdown.len()]);
}

#[test]
fn details_inside_list_items_and_block_quotes() {
    let markdown = "- item\n\n  <details>\n  <summary>Nested</summary>\n\n  Body\n\n  </details>\n\n> <details><summary>Quoted</summary>\n>\n> Inside\n>\n> </details>";
    let result = render_markdown(markdown, &options(40).with_details_open(BTreeMap::new()));
    assert_eq!(
        rows(&result),
        ["•  item", "", "   ▶ Nested", "", "│ ▶ Quoted"]
    );
    assert_eq!(
        result
            .contributions
            .iter()
            .filter(|item| matches!(item.kind, MarkdownContributionKind::Details { .. }))
            .count(),
        2
    );
}

#[test]
fn plain_text_previews_drop_comments_and_scripts() {
    assert_eq!(
        bmux_markdown::markdown_to_plain_text(
            "<!--\nhidden\nnote\n-->\nShown <b>bold</b> &amp;\n\n<script>\nno()\n</script>\n\ndone"
        ),
        "Shown bold & done"
    );
}
