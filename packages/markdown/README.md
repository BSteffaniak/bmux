# bmux_markdown

Reusable Markdown-to-terminal projection using canonical `bmux_tui::Line`,
`Span`, and geometry types. Extracted from Bcode's markdown-render crate without
rewriting rendering behavior. Applications retain interaction and resource authority.

## API handoff

- `parse_markdown_document` returns parser-neutral semantic events and original
  UTF-8 byte ranges, including images, links, tables, math, alerts, and code fences.
- `render_markdown` takes `MarkdownRenderOptions` and returns `MarkdownRenderResult`:
  terminal lines, semantic contributions, contribution/code geometry, and anchors.
- `MarkdownRenderResult` lazily retains selection provenance, code selections,
  and layout signatures; `has_selection_source` checks canonical source identity.
- `MarkdownStreamingRenderState` reuses derived rendering state during streaming.
- `render_markdown_lines` and `markdown_to_plain_text` provide text-only projections.
- `MarkdownTheme`, caller-supplied document context, and destination classification
  are reusable presentation inputs, not resource loading or activation policies.

Images retain source/alt metadata and geometry while the text fallback remains
`[image: description]`. The caller owns fetching, decoding, image lifecycle,
terminal placement, link activation, selection/export effects, and slide state.
Mermaid contributions expose source/output/fallback metadata without installing
images or creating application state. Syntax rendering uses `bmux_syntax_render`.

## Compatibility and ownership

All upstream fixtures and snapshot bodies are retained. Source mappings refer to
canonical Markdown, not fallback strings reconstructed from terminal cells.
The compatibility adapter's private `bcode-contribution:*` sentinel links are intentionally retained:
these are parser/projection-adapter markers,
not Bcode services or public activation targets. The original palette, destination
rules, and Markdown extensions remain compatible. GitHub context is optional,
caller-supplied destination metadata; no network access occurs here.

The extraction baseline uses HyperChad commit
`82547e2d932e8ff4dc181808e9e58f0f63ce16e5`. Inspection found its Markdown,
color, and transformer directories (including transformer models) byte-identical
to BMUX's `5626b800db0d31deea3b4e1c16880bc2c4080cba`. Keep the existing coherent
BMUX workspace URL/revision; no parser revision update is needed on this evidence.
`hyperchad_markdown` needs workspace
`xss-protection` plus this crate's `syntax-highlighting` feature.

## Lint exception scope

The user explicitly authorized only `clippy::multiple_crate_versions` at this
crate's root for incompatible upstream dependencies. HyperChad uses strum 0.28
and thiserror 2; its unconditional Free Log client/models use strum 0.26 and
thiserror 1. HyperChad's proptest/getrandom 0.3 branch also retains r-efi 5 while
tempfile/jobserver's getrandom 0.4 uses r-efi 6. Lock-only unification cannot fix
these incompatible requirements. Preserve the existing parser graph and fastrand
2.3 lock selection; do not downgrade unrelated dependencies to silence this lint.
All other crate-level lints remain enabled. The `render_markdown` function alone
has a documented `too_many_lines` exception to keep canonical source, projected
source, provenance, and diagnostic stage ordering readable together.

## Validation

Run the workspace's required Nix clippy, nextest, and machete checks after workspace
registration. The extraction lane deliberately did not run Cargo/build/test/fmt
commands because the coordinator owns the shared target and lockfile. Regression
coverage includes all original unit tests, Markdown fixtures, snapshots, and a
consumer-facing canonical-TUI/source-boundary test.
