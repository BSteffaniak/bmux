# Unified plugin presentation plan

One retained scene contract for everything plugins (and the attach host) draw,
replacing the separate rendering paths that exist today. The contract must express
everything every current path can express, plus typed interaction and
host-resolved placement, and must cross a process boundary.

## Current state (inventory)

Four paths draw non-pane UI in the attach client today.

| Path | Types | Producers | Rendered natively? |
| --- | --- | --- | --- |
| A. Plugin surfaces + layouts | `bmux_plugin::surface::PluginSurface` (`Vec<RenderOp>`), `bmux_plugin::layout` | sidebar, tab bar (strip, menu, toasts), host notification (`local_presentation.rs`) | yes |
| B. Portable frontend presentations | `bmux_tui::presentation::portable::PortablePresentation` | none in tree (embedder API only) | no: observers and headless clients only |
| C. Render extensions | `bmux_plugin::render::AttachRenderExtension`, `RenderLayerScene` | decoration renderer, pane text redaction, visual adapters | yes |
| D. Host overlays | `AttachSurface` + `RenderOp` lowered straight into `RetainedSurface` | prompts, help, damage overlay | yes |

### What each path can express

| Capability | A | B | C | D |
| --- | --- | --- | --- | --- |
| Text runs, styled spans, fill, clear, erase, border ops | yes | via cells | yes | yes |
| Per-cell transparency (`RenderCell::sparse`) | yes | no (opaque buffer) | yes | yes |
| Grapheme clusters, wide characters | no (`char`) | yes (`Buffer`) | no (`char`) | via ops |
| Under pane content (shows through transparent pane cells, keeps pane background) | no | no | yes (`BeforePaneContent`) | no |
| Over pane content, per pane | no | no | yes (`AfterPaneContent`) | no |
| Surface chrome ownership (`surface_chrome`) | no | no | yes | consumer |
| Pane content inset (`content_rect_override`) | no | no | yes | no |
| Pixel graphics in one z stream with text (`TerminalGraphicOverlay`) | no | no | yes | no |
| Images with occlusion (`ImageContribution`) | no | yes | no | no |
| Keyed items, diffed by key; sampling skipped by revision | no | no | yes | no |
| Layout participation (split/overlay/hidden, viewport insets) | yes | no (fixed area) | no | no |
| Interactive regions with pointer shape, focusable, capture | yes (string ids) | hits with roles (string ids), no host routing | coarse hooks (no geometry) | private host `HitMap` |
| Modal input scope | yes | modal scopes | no | host |
| Terminal cursor | no | yes | no | yes |
| Selection / semantics / copyable text | yes | yes, plus metadata availability | no | no |
| Pane text rules (redaction) | no | no | yes | no |
| Visual adapters (frame observation and projection) | no | no | yes | no |

### Defects found while mapping

- On full-scene frames, path A surfaces are painted in `BTreeMap<Uuid>` order, not
  `(layer, z)` order (`runtime.rs`, `retained_plugin_repaints`). Fixed in phase 2.
- All path A surfaces paint after every scene surface regardless of layer or z,
  although hit testing and occlusion use layer and z.
- Path A paint clips to `surface.rect` and ignores `clip_rect`. Fixed in phase 2.
- `retained_surface_metadata_equal` ignores `modal`.
- Path B never reaches the native terminal, and its hits are never routed back.
- Tab-bar menus and toasts recompute absolute rects from the viewport themselves.
  Fixed in phase 3.
- Every producer parses string hook ids (`strip_prefix("bmux.sidebar:sidebar:tab:")`).
  Fixed in phase 4.
- Under-content glyphs dropped named colours (`render_color_to_cell_color` mapped
  `RenderColor::Named` to the default colour). Fixed in phase 0; the parity golden
  `pane_extension_layers` records the corrected colour.
- Ops cannot inherit what is beneath them: `Canvas::text` turns a `None` colour into
  the terminal default, so a text run over a filled surface resets the fill's
  background (visible in the `sidebar_split` golden). This is kept as the
  contract's meaning (changing it would restyle every producer); show-through is
  explicit through transparent cells.

## Target design

### One contract, owned by a neutral crate

The contract types live in the neutral crate `bmux_presentation_scene`
(`packages/presentation-scene`): `paint` holds the drawing vocabulary moved out
of `bmux_plugin::render` (which re-exports it for existing callers), and `scene`
holds placement, cell blocks and typed hits. Everything is plain serde data with
no runtime, so the same types are published in process and encoded across a
plugin boundary. (`bmux_scene_protocol` is unrelated: it is the decoration
plugin's server protocol.)

A publication from one owner is a snapshot of scenes:

```text
SceneSnapshot { revision, scenes: Vec<Scene> }
Scene {
    id: SceneId { owner, local, retained },
    revision,
    target: SceneTarget,
    layer, z, opaque, modal, visible, accepts_input,
    clip: Option<LocalRect>,
    items: Vec<SceneItem>,        // keyed, z-ordered paint stream
    hits: Vec<SceneHit>,          // typed, revision-checked interaction
    cursor: Option<SceneCursor>,
    focus: SceneFocus { scope, modal_scopes },
    metadata: Option<SceneMetadata>,  // selection, semantics, text, availability
}
```

### Targets: where a scene lives

```text
SceneTarget::Root(Placement)
SceneTarget::Pane { surface: Uuid, depth: PaneDepth }

Placement::Split { layout: LayoutId, order, edge, extent }   // consumes the remainder
Placement::Overlay { order, anchor: Anchor, size: Size, margin: Insets }
Placement::AnchoredTo { region: (SceneLocal, HitKey), side: Side, size: Size, flip: bool }
Placement::Fill { order }                                     // the whole remainder
Placement::Hidden { layout: LayoutId, order }

PaneDepth::UnderContent   // shows only through transparent pane cells, keeps pane background
PaneDepth::OverContent
PaneDepth::Chrome         // border/inset area; may declare a content inset
```

The host resolves every placement. Producers never compute absolute rects.
`AnchoredTo` may only name the producer's own scenes. Allocation callbacks fire
only when an allocation changes.

### Items: what a scene draws

```text
SceneItem { key: ItemKey, z: i16, kind: ItemKind }
ItemKind::Ops(Vec<RenderOp>)          // text, spans, fill, clear, erase, border, cell grid
ItemKind::Cells(CellBlock)            // grapheme-aware cells with per-cell transparency
ItemKind::Graphic(TerminalGraphicOverlay)
ItemKind::Image(ImagePlacement)
```

- Coordinates are scene-local. Ops and rects may be bounds-relative
  (`Edge::{Start, End}` offsets, `Extent::Fill`), so scenes that only stretch with
  their allocation do not republish on resize.
- `CellBlock` uses the `bmux_tui::Buffer` cell model (symbol, width, wide
  continuation) and adds a transparency flag per cell, so it is a superset of both
  `RenderCell::sparse` and `PortablePresentation::cells`.
- Graphics and images share the scene's z stream with text. The host keeps the
  existing order guarantee (pending text flushes before a graphic).
- Style is `RenderStyle`. A `None` colour keeps today's meaning (the terminal
  default), so existing producers render unchanged; show-through is expressed
  only by transparent cells (`SceneCell { symbol: None }`, sparse `RenderCell`).
  Underline style/colour and hyperlinks are reserved for later.
- The host diffs items by key (as `extension_retained` does for path C today), so
  revision skipping and keyed damage apply to every producer.

### Hits: typed, revision-checked interaction

```text
SceneHit { key: HitKey(u64), rect, role, pointer, focusable, tab_order, enabled, hover, capture }
```

On the wire a hit key is an integer. Producers keep a typed table for each
published revision (`HitTable<K>`: `K -> HitKey` while painting, `HitKey -> K`
when input arrives). Input events carry `(scene, revision, HitKey)`; a key from a
revision the producer no longer holds is rejected instead of being misrouted.
Components generate hit keys and tables automatically.

### Facets: declarations that filter pane content

Facets are not drawing. They travel in the same snapshot, versioned with it:

- `ContentInset { surface, insets }` (today `content_rect_override`)
- `TextRules` (today `pane_text_rules`)
- `FrameObservation` requests (today visual adapters)
- `InputHooks` (today coarse extension hooks; kept for producers that need
  pane-context key and mouse filters without geometry)

### Host side

- One `SceneRegistry` per attach runtime, holding the latest snapshot per owner,
  bounded by the existing limits (cells, bytes, regions, scenes per owner, update
  rate).
- One resolver: placements to rects, using `resolve_plugin_layout` extended with
  anchors.
- One lowering path: scene items to canvas text, cells and images, used for root
  and pane targets. Root scenes are painted in `(layer, z)` order interleaved with
  the scene surfaces. Pane scenes are painted in the existing under-content and
  after-content stages.
- One router: hit testing over all scenes by `(layer, z)`, modal floors, capture,
  then delivery through the owner's transport.

### Transports

- In process (bundled plugins and the attach host): publish the `Scene` structs
  directly into the registry; input handlers are closures
  (`AttachPresentationInputRegistry`).
- Out of process: the same `SceneSnapshot` encoded (the existing frontend
  presentation request path, versioned contract id), input delivered as encoded
  events through the existing `AttachInputCodec` service path.

## Migration phases

Each phase lands with the parity harness and the targeted tests green.

0. **Inventory, harness, baselines.** This document; golden full-frame tests for
   every path (`packages/cli/src/runtime/attach/presentation_parity_tests.rs`);
   perf baselines from `scripts/perf-render.py`.
1. **Contract types** in `bmux_presentation_scene` with serde: the paint
   vocabulary moved out of `bmux_plugin::render`, plus placement resolution
   (`resolve_anchored`, `resolve_attached`), `CellBlock`, `HitKey`/`HitTable`.
2. **Unified root lowering and ordering.** `RenderOp::Cells(CellBlock)` gives every
   path grapheme clusters, wide symbols and per-cell transparency (lowered by the
   attach pipeline, flattened only when every cell is a single scalar, clipped to
   occluders by the decoration renderer, validated on publish). Plugin surfaces
   paint in `(layer, z)` order and are clipped to their `clip_rect` on every
   frame. Graphics and images on root scenes move to phase 5 with keyed items.
3. **Placement.** `PluginSurfaceTarget::Overlay`: `Anchored` (corner, edge or
   centre of the remaining area or the whole viewport, with size and margin) and
   `Attached` (next to a region of another of the owner's surfaces, aligned,
   flipping when the preferred side has no room, kept on screen). The host
   resolves both (`resolve_plugin_surface_rects`); publishing rejects attachments
   to unknown surfaces. The tab-bar menu (attached to its tab), its dismiss
   backdrop and toasts (anchored) and the host notification (anchored to the
   bottom row) no longer compute positions. Allocation callbacks stay as they are:
   they fire only when retained surfaces are reconciled, and repeat delivery lets a
   producer retry a rejected publication.
4. **Typed hits.** Regions carry an optional `HitKey`; every input event the host
   delivers carries `AttachInputTarget { surface, region, revision, key }`.
   Producers keep a `HitTables<K>` per published (applied) revision and resolve
   the key against the revision the event names, so input for a retired revision
   resolves to nothing. Events without a key (stateless or out-of-process
   callers) resolve the producer's own owner-local region id through one
   symmetric codec. The sidebar, tab bar, tab menu and workspace rename no longer
   parse `owner:surface:region` hook strings; `hook_id` remains only as the
   coarse address of decoration hooks and remote codecs.
5. **Pane targets and facets.** Under/over/chrome depths painted through the
   existing pane stages; decoration renderer and redaction publish scenes and
   facets instead of implementing `AttachRenderExtension`.
6. **Out-of-process transport.** Frontend presentations publish scenes; the
   native terminal renders them; input is routed back. A test plugin proves a
   split, an anchored overlay, an under-content layer and clicks.
7. **Host overlays and components.** Prompts and help publish scenes; stock
   components (list, tab strip, menu, toast stack, text input) replace the
   sidebar's and tab bar's hand-written measuring, hover and scrolling.
8. **Remaining special cases.** Committed-frame subscription and attach context
   snapshot as generic primitives; broadcast and pane-runtime commands move onto
   presentation commands; the old paths and adapters are deleted.

## Parity harness

`presentation_parity_tests.rs` renders fixed scenes through the real attach frame
renderer (`render_attach_frame_to_writer`), replays the bytes into a
`TerminalGridStream`, and compares a textual dump of every cell's text and style
against checked-in goldens under `packages/cli/tests/fixtures/presentation-parity/`.
Set `BMUX_UPDATE_PARITY_GOLDENS=1` to rewrite them. A golden may only change in a
commit that explains why (a fixed defect, never an unexplained drift).

Scenarios:

- split layout surface with border, fill and text (sidebar shape)
- bottom strip plus modal menu and toast over it (tab bar shape)
- overlapping surfaces with different layer and z (ordering)
- host notification on the bottom row
- under-content glyphs beneath transparent and background-only pane cells
- after-content ops including sparse cell grids
- graphics overlays (checked through the emitted image placements)

## Performance

- Gate: `scripts/perf-render.py check` against `docs/perf-baselines/render`.
- Path A and future pane scenes get a bench scenario in `pipeline_bench.rs` so
  regressions in the unified lowering are visible.
- Lowering cost is proportional to changed item keys, not to scene size.
