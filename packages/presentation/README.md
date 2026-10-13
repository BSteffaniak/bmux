# bmux_presentation

Portable, source-independent presentation content and canonical BMUX components.
Inspired by Presenterm, not syntax- or behavior-compatible with it. No alternate
terminal, wrapping, ANSI, hit-testing, or geometry engine is introduced.

## Try a self-contained deck

With the presentation plugin enabled, run from the repository root:

```sh
cargo run -p bmux_cli -- present check packages/presentation/examples/quickstart.md
cargo run -p bmux_cli -- present play packages/presentation/examples/quickstart.md
```

Add `--presenter` to playback to show speaker notes. The quickstart includes
code, columns, reveals, and live reload without images, Mermaid, or custom
component registration. `examples/deck.md` is a richer authoring fixture that
requires external assets and component registration; it is not self-contained.

## Public interfaces

- `Deck { id, metadata, slides }`, `Slide { id, blocks, reveal_steps, notes }`,
  `Block { id, reveal_step, source, kind }`: serde-capable inert content.
- `DeckBuilder::new(id).slide(slide).build()` validates nonempty/unique IDs.
  `SlideBuilder` supports text, Markdown, code, columns, custom blocks, pauses,
  notes, and arbitrary `BlockKind` via `block(id, kind)`.
- `parse_markdown(source_name, source) -> Result<Deck, MarkdownError>` records
  original UTF-8 byte ranges and one-based line/column diagnostics. It performs
  no filesystem access, network fetching, subprocess execution, or component
  activation.
- `PresentationState::new(&deck)` and `reduce(&deck, &mut state, action)` own no
  runtime. `NavigationAction` is serde-capable. Next/previous traverse cumulative
  reveals and reverse each other across slide boundaries. `Reconcile` preserves
  the selected slide ID on reload and clamps reveals; deleted IDs fall back to
  the first slide. Hosts reset `ViewerState` scroll when changing slides.
- `SlideComponent::new(&slide, step, &theme, &registry)` implements BMUX
  `Component`. Hidden content reserves measurement but paints/registers/handles
  nothing by default. `.hidden_content(HiddenContent::Collapse)` opts out.
- `ViewerComponent::new(&deck, &navigation, &viewer_state, &theme, &registry)`
  adds a status footer and canonical interactive scroll viewport. Overflow
  remains scrollable rather than silently discarded. Custom control events are
  offered before viewport scrolling; the host owns navigation/key bindings and
  focus-target dispatch. `.focused_block(&BlockId)` routes keyboard/paste only to
  the explicitly selected custom block; without it no custom block receives
  keyboard/paste. Nested column wrappers preserve the same targeting.
- `SpeakerNotesComponent { slide, theme }` is an explicit local-only component,
  never included in the audience viewer.
- `PresentationTheme::{dark, light, named}` provides application styling.

## Markdown dialect

Front matter is YAML between opening/closing `---` (or closing `...`). Supported
metadata keys: `id`, `title`, `author`, `theme`; other keys remain inert metadata.
Outside front matter, only **top-level** Markdown thematic breaks separate slides.
Rules inside fences, blockquotes, or lists do not separate slides.

Directives are standalone, namespaced HTML comments:

```markdown
<!-- bmux:slide-id introduction -->
<!-- bmux:block-id stable-heading -->
# Introduction

<!-- bmux:pause -->
This appears on the next reveal.

<!-- bmux:columns 1,2 -->
Left column
<!-- bmux:column -->
Right column
<!-- bmux:end-columns -->

<!-- bmux:notes -->
Private speaker notes, including Markdown rules.
<!-- bmux:end-notes -->

<!-- bmux:component linked-chart {"series":"demo"} -->
```

Use explicit IDs for long-lived slides/blocks. Automatic IDs are deterministic
content-derived keys with duplicate ordinals: unrelated insertions do not change
unique-content IDs, but changing the identity-bearing first block changes an
implicit slide ID. Editing duplicate content requires explicit IDs for continuity.
Column weights must be positive and every declared column needs its separator;
nested directive columns are rejected (Rust models may compose nested columns).
Unknown/malformed namespaced directives and unclosed groups are source-located
errors. Non-namespaced HTML remains inert Markdown content. Code, GFM tables,
standalone images, and Mermaid fences become portable typed blocks. Mixed image
paragraphs remain Markdown and use the shared renderer's readable fallback.

## Linked components and rich assets

`CustomRegistry::register(name, factory)` rejects duplicate/empty names.
`CustomComponentFactory::build<'a>(&'a self, &'a Block)` returns
`Result<Element<'a>, String>`; factories may borrow caller-owned canonical
control state. `revision()` invalidates disposable caches when external state
changes. `OwnedFactory::new` adapts factories of owning components. No dynamic
Rust loading exists. Unknown factories produce visible diagnostics.

`RichContent::build<'a>(&'a self, &'a Block) -> Option<Element<'a>>` receives code,
image, or Mermaid blocks; `revision()` describes provider-owned revisions.
Code providers return their prepared canonical component directly, without a
second highlighting or Markdown wrapper. Returning `None` for code uses the
shared Markdown renderer fallback.
Attach with `.rich_content(&provider)` on slide/viewer components. Providers must
only use already-authorized/resolved assets and remain deterministic/read-only.
Returning `None` uses readable alt text or Mermaid code fallback, reserving at
least the block's requested rows. `ImageComponent::new(id, ImagePayload, rows)`
paints supplied PNG/raw pixels through scoped `PaintCx::push_image`, preserving
canonical transforms, clipping, and frame lifecycle. Rasterized Mermaid may use
that same component; text Mermaid output may use canonical `TextBlock`. Runtime
owns rendering/decoding policy, limits, cancellation, and asset authorization.

## Audience security boundary

`SpeakerNotes` is skipped by serde. `Deck::public()` additionally removes notes,
all nested diagnostic source locations, and all custom configuration. Local deck
objects retain those values for diagnostics and linked applications.

**`public()` is not a complete confidentiality or asset authorization boundary.**
Authored raw Markdown (including inline destinations/HTML), `Image.destination`,
text/code/Mermaid, metadata, IDs, and custom names may contain local paths or
other sensitive authored information. The runtime must review/redact these
fields and rewrite authorized assets to opaque IDs before transport. Do not
serialize a local deck directly to an audience or activate custom components
received from peers. Public custom config is deliberately `null`; host-owned
linked components need an explicitly reviewed public configuration projection.

## Examples and validation

`examples/deck.md` is an authoring fixture covering metadata, reveals, notes, code,
tables, columns, images, Mermaid, and a linked custom component. Its
`asset:architecture` reference requires a caller-supplied authorized asset; it is
not a standalone CLI deck. `examples/linked_deck.rs` builds a Rust
deck, registers a canonical interactive button with caller-owned state, injects
an authorized pixel asset, and stages an offscreen canonical frame. A runtime
presents that frame and routes events against the same resolved layout.

Focused tests cover parsing/source ranges, stable IDs, cumulative inverse
navigation, reload reconciliation, private projection, canonical reveal
measurement, image transforms, custom registration, and overflow composition.
See the repository's `AGENTS.md` for required toolchain and validation commands.
