# bmux_web_renderer

Directional browser output for `bmux_tui::presentation::PreparedPresentation`.
Native transport, clients, CLI, plugin loading, and product state are not dependencies.
The compositor remains the only authority; browsers consume its final buffer and
shared cursor/hit/focus/selection/semantic/image/damage vocabulary.

## Host API and wire v2

Wire v2 requires `unsupported_metadata`: a bounded list of the typed
`MetadataKind::{Selection, Semantics}` values, serialized as `selection` and
`semantics`. V1 is explicitly rejected: it cannot establish whether an empty
metadata scene means available-but-empty or absent producer support. Unknown,
duplicate, over-budget, or missing availability declarations are rejected.
`from_prepared` defaults to complete standalone metadata (`[]`). Native projections
whose producer does not supply logical metadata must explicitly call
`with_unsupported_metadata(&[MetadataKind::Selection, MetadataKind::Semantics])`.
Unavailable kinds cannot also contain scene entries. The browser still renders
faithfully available output and displays unavailable metadata; it does not fabricate
logical content or reject cells/images merely because metadata is unavailable.

`selection_text` is the exact `BTreeMap<SelectionContentId, String>` supplied by
the producer; no text is reconstructed from output cells. Use
`with_selection_text(&source_map)?` to declare text available. Mandatory
`selection_text_available` distinguishes absent source text from an empty supplied
map. Prepared standalone frames with selection fragments initially declare text
unavailable until this builder runs; standalone frames without fragments have a
complete empty map. The example supplies its real title source and fragments.
Keys are bounded to 1024 bytes; maps have at most 4096 entries, 1 MiB aggregate
UTF-8 text and 64 KiB per value. Duplicate wire keys, missing referenced content,
and out-of-range/non-UTF-8-boundary source/word/line ranges are rejected when text
is supplied. Committed metadata includes the source map and availability. Producer
namespaced keys must match the scene's content IDs exactly; retained provenance
continues to identify owner-local surface/revision rather than a browser authority.
`inspect_json` and committed `metadata_json` expose this list. Every retained
`SurfaceProvenance` also requires `selection_available` and `semantics_available`.
These source-owned flags do not default to true. Global `unsupported_metadata`
means no producer supplies that category; absence from that list does **not** imply
complete coverage when a retained surface declares it unavailable. Derived
`partial_metadata` in inspection/committed metadata and the browser status makes
mixed coverage explicit. Native terminal-grid surfaces remain unavailable until
their logical producer supplies metadata. A true per-surface flag contradicting a
global unsupported declaration is rejected. No metadata is inferred from cells.

```rust,ignore
let frame = bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
    generation, revision, prepared,
)?.with_surfaces(authoritative_surface_provenance)?;
let json = bmux_web_renderer::encode_json(&frame)?;
```

`from_prepared(revision, prepared)` uses generation `standalone`.
Revisions are nonzero monotonic `u64` **within a generation**. Hosts must change
the generation on reattach/source replacement, even if revisions restart.
`SurfaceProvenance { retained_id: String, owner_plugin_id: Option<String>,
owner_local_id: Option<String>, revision: Option<u64>, stream_end: Option<u64>,
rect: Rect, clip: Rect,
layer: i16, z: i32, content_id: Option<String> }` is optional bounded descriptive
projection from the authoritative retained compositor, not browser-composed state.
UUID identities are opaque text, without an attach/native transport dependency.
Producer ordering, absent ownership/revisions, original allocation, native clip,
layer, z and optional content identity/continuation watermark are preserved exactly.
`stream_end: None` does not imply completeness; the browser never guesses a
watermark or reconstructs retained content via producer callbacks. There is no invented
parent graph. Allocations and clips may extend offscreen; visible intersections
are bounded without rewriting descriptive geometry. The host must copy these
facts from its native retained surfaces; the renderer cannot recover them from pixels.

JSON is the serde representation of `BrowserFrame`, with these exact top-level
fields: `version` (1), `revision`, `generation`, `surfaces` (defaults to empty),
`buffer`, `cursor`, `hits`, `focus_scope`, `focus`, `selection`, `semantics`,
`image_scene`, `damage`. All non-envelope models are existing TUI primitives.
This is a full repaired snapshot, including retained cells and reconciled images,
not a delta stream. Image scene omission/removal is honored even after frame gaps.
Unknown versions, unknown envelope fields, unsupported payload variants/modifier
bits, bad geometry/wide spans/focus/selection, and bounded-resource violations are
rejected. PNG IHDR dimensions are checked before browser decoding; decoded dimensions
are checked again. RGB8/RGBA8 lengths must match dimensions. No decoded frame can
install `PresentationState` or `PreparedPresentation`.

Default limits: 16 MiB JSON, 262144 cells, 4096 metadata regions, 128 images,
8 MiB aggregate image bytes, 4194304 aggregate image pixels, 128 bytes per symbol.
Canvas additionally limits surfaces to 16384 pixels per axis and 16777216 pixels.
These are hard rejects, never partial snapshots presented as complete output.

`BrowserBackend::render(frame, previous)` and `reset()` are replaceable neutral
backend hooks. `Renderer<B>::present` validates bounds and generation/revision,
then advances its committed snapshot only on backend success. Failed render/reset
preserves committed interaction metadata. An explicit successful reset permits a
new generation. Backends own their resources and must release them on reset/drop.

## Browser integration

WASM feature `wasm` exports `CanvasRenderer`:

- `new CanvasRenderer(canvas_id, cell_width, cell_height)`
- `CanvasRenderer.inspect_json(json)` returns validated generation and decimal revision
- `await renderer.present_json(json)` commits only after resource decode and Canvas copy
- `renderer.revision()` returns decimal text (no JavaScript u64 precision loss)
- `renderer.metadata_json()` returns committed metadata and surface provenance
- `renderer.reset()` clears resources/output/committed revision
- `renderer.free()` releases Rust/bitmap ownership

Calls must be serialized. Canvas stages complete text, backgrounds, images and
cursor offscreen before the visible copy; superseded/removed/pending bitmaps are
closed. A repair copy restores the old surface if the final visible copy fails
following resize. Browser loss preventing restoration remains an output failure,
not a metadata commit. Text follows TUI physical cell widths; wide continuations
are not redrawn, glyphs are clipped to their shared cell span. ANSI/256/RGB colors,
bold/italic/dim/reverse/hidden/underline/strike are supported. Blink is rendered in
its visible phase (no animation); cursor is a steady underline. Font rasterization
is browser monospace, not guaranteed native font parity. Selection highlights
already belong to the final buffer. Metadata is read-only; no remote input API.

Serve exactly these assets at the same localhost origin:

| Route | Source |
| --- | --- |
| `/index.html` | `browser/index.html` |
| `/bootstrap.js` | `browser/bootstrap.js` |
| `/bmux_web_renderer.js` | wasm-bindgen generated JS |
| `/bmux_web_renderer_bg.wasm` | wasm-bindgen generated WASM |

Bootstrap with `/index.html#token=<percent-encoded bearer token>`. The script
captures the token in memory and immediately strips the fragment with
`history.replaceState`. No storage, credential logging, cross-origin fetch,
redirects, cookies, or CORS are needed. It issues authenticated same-origin
`GET /snapshot`, then `/snapshot?after=<committed transport cursor>`:

- 200: full BrowserFrame JSON and canonical positive u64 `X-Snapshot-Revision` header
- 204: long-poll timeout, retry serially
- 503: source unavailable/closed, retry full snapshot (new generation may arrive)
- 401/403: erase token and require a fresh bootstrap URL

Both browser response streaming and Rust decoding bound input. The header is an
independent transport publication counter, not the native frame revision. Polling
advances its cursor only after output commits or an already committed same-generation
native revision is recognized. Header values use canonical decimal/BigInt u64 checks
and must advance within the host. A failed output retries a full snapshot and may
receive the same transport publication; regression is rejected. Native generation
changes may restart native revisions without restarting the transport counter.
Poll errors trigger full repair; unchanged native revisions are skipped. A changed validated generation
uses transactional full-snapshot repair; failed replacement preserves old output and metadata. The host owns endpoint security,
authorization, token generation and exact asset allowlisting.

## Build and validation (coordinator)

Use the repository's locked Nix toolchain and target directory. No installs needed
when the matching `wasm-bindgen` 0.2.126 CLI is already supplied:

```sh
bash scripts/dev.sh cargo test -p bmux_web_renderer
bash scripts/dev.sh cargo clippy -p bmux_web_renderer --all-targets -- -D warnings
bash scripts/dev.sh cargo build -p bmux_web_renderer --features wasm --target wasm32-unknown-unknown
# Resolve the workspace target directory under the selected Nix environment.
target/web-tools/bin/wasm-bindgen --target web --out-dir <host-assets> \
  <workspace-target>/wasm32-unknown-unknown/debug/bmux_web_renderer.wasm
# Copy browser/index.html and browser/bootstrap.js into that same asset directory.
bash scripts/dev.sh cargo run -p bmux_web_renderer --example snapshot
```

The standalone example emits the exact full JSON without a BMUX runtime and includes
Unicode, styles, hits, semantics, cursor and RGBA pixels. Full live BMUX integration
must export the same native retained preparation path (including plugin UI/images)
and attach native surface provenance; this crate never duplicates that compositor.
Repository-wide clippy, nextest and machete remain coordinator validation gates.
