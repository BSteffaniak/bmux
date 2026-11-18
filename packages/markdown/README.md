# bmux_markdown

Reusable Markdown-to-terminal projection using canonical `bmux_tui::Line`,
`Span`, and geometry types. Extracted from Bcode's markdown-render crate without
rewriting rendering behavior. Applications retain interaction and resource
authority.

## API handoff

- `parse_markdown_document` returns parser-neutral semantic events and original
  UTF-8 byte ranges, including images, links, tables, math, alerts, and code fences.
  `parse_markdown_document_with` takes `MarkdownParseOptions` (smart punctuation).
- `MarkdownRenderOptions::with_presentation` takes a `MarkdownPresentation`:
  unordered list bullet, marker gap, code block frame (`Framed` or `None`),
  blockquote bar, table header alignment, and smart punctuation. Defaults match
  the standard terminal look; render, provenance, and streaming parses all use
  the presentation's punctuation setting.
- `render_markdown` takes `MarkdownRenderOptions` and returns `MarkdownRenderResult`:
  terminal lines, semantic contributions, contribution/code geometry, and anchors.
- `MarkdownRenderResult` lazily retains selection provenance, code selections,
  and layout signatures; `has_selection_source` checks canonical source identity.
- `MarkdownStreamingRenderState` reuses derived rendering state during streaming.
- `render_markdown_lines` and `markdown_to_plain_text` provide text-only projections.
- `MarkdownTheme`, caller-supplied document context, and destination classification
  are reusable presentation inputs, not resource loading or activation policies.

## Raw HTML (`html` feature)

Markdown may embed raw HTML. By default it is shown as text (dangerous tags
escaped). The off-by-default `html` Cargo feature adds `MarkdownHtmlMode` and
`MarkdownRenderOptions::html` / `with_html`:

- `Render` (the default with the feature): the element and attribute subset
  GitHub keeps after sanitizing. Inline `b strong i em cite dfn var code tt samp
  kbd mark ins u del s strike sub sup q small abbr span ruby rt rp br wbr`;
  blocks `p div h1`–`h6 ul ol li dl dt dd blockquote pre hr table thead tbody
  tfoot tr th td caption figure figcaption details summary`; `img` and
  `picture`/`source`. Attributes read: `href src srcset media alt title width
  height align open colspan`. Comments and `script style iframe object svg
  template textarea title …` are removed with their contents; other tags are
  removed and their text kept; character references are decoded with the full
  HTML5 table.
- `Strip`: every tag is removed and its text kept; blocks still start rows and
  `<br>` still breaks; comments and script-like elements are removed.
- `Raw`: today's text presentation, including the `<details>` projection.

Rendering maps elements onto the existing renderer: bold/italic/code/strike,
`kbd` and `mark` through `MarkdownTheme::keyboard` and `highlight`, `ins`/`u`
underline, `sub`/`sup` as Unicode scripts when every character has one (else
`_x`/`^(xy)`), headings without document anchors, HTML tables with their own
column alignment (`align` on the first row's cells) and `colspan` as empty
cells, and `p`/`div`/`h*` `align="center|right"` shifting each measured row and
its geometry. Links and images become `Link`/`Image` contributions with
`MarkdownLinkKind::Html`, destinations classified by
`resolve_markdown_destination` under the document context; `img` `width` and
`height` are exposed on `MarkdownImage`. A `<picture>` shows its fallback `img`,
else a source without a media condition. `<details>` keeps interactive
disclosure with the same contribution identities (`details:<start>:<end>`) and
`open` default as the raw projection; Markdown after a blank line inside an
element (for example a `<details>` body) belongs to it. HTML block lines are
stitched together, so tags and comments may span lines; unclosed elements close
at the end of their container and stray end tags are ignored.

Selection provenance maps verbatim HTML text to its source bytes; text hidden
by a collapsed `<details>` or transformed (`sub`/`sup`) claims no cells.

Images retain source/alt metadata and geometry while the text fallback remains
`[image: description]`. The caller owns fetching, decoding, image lifecycle,
terminal placement, link activation, selection/export effects, and slide state.
Mermaid contributions expose source/output/fallback metadata without installing
images or creating application state. Syntax rendering uses
`bmux_syntax_render`.

## Compatibility and ownership

All upstream fixtures and snapshot bodies are retained. Source mappings refer to
canonical Markdown, not fallback strings reconstructed from terminal cells.
The compatibility adapter's private `bcode-contribution:*` sentinel links are
intentionally retained:
these are parser/projection-adapter markers,
not Bcode services or public activation targets. The original palette,
destination
rules, and Markdown extensions remain compatible. GitHub context is optional,
caller-supplied destination metadata; no network access occurs here.

The extraction baseline uses HyperChad commit
`82547e2d932e8ff4dc181808e9e58f0f63ce16e5`. Inspection found its Markdown,
color, and transformer directories (including transformer models) byte-identical
to BMUX's `5626b800db0d31deea3b4e1c16880bc2c4080cba`. Keep the existing coherent
BMUX workspace URL/revision; no parser revision update is needed on this
evidence.
`hyperchad_markdown` needs workspace
`xss-protection` plus this crate's `syntax-highlighting` feature.

## Lint exception scope

The user explicitly authorized only `clippy::multiple_crate_versions` at this
crate's root for incompatible upstream dependencies. HyperChad uses strum 0.28
and thiserror 2; its unconditional Free Log client/models use strum 0.26 and
thiserror 1. HyperChad's proptest/getrandom 0.3 branch also retains r-efi 5
while
tempfile/jobserver's getrandom 0.4 uses r-efi 6. Lock-only unification cannot
fix
these incompatible requirements. Preserve the existing parser graph and fastrand
2.3 lock selection; do not downgrade unrelated dependencies to silence this
lint.
All other crate-level lints remain enabled. The `render_markdown` function alone
has a documented `too_many_lines` exception to keep canonical source, projected
source, provenance, and diagnostic stage ordering readable together.

## Validation

Run the workspace's required Nix clippy, nextest, and machete checks after
workspace
registration. The extraction lane deliberately did not run Cargo/build/test/fmt
commands because the coordinator owns the shared target and lockfile. Regression
coverage includes all original unit tests, Markdown fixtures, snapshots, and a
consumer-facing canonical-TUI/source-boundary test.
