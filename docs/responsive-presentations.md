# Responsive presenter audiences

## Explicit delivery contracts

Presenter audiences select semantic delivery with the announced presenter URL:

```text
?delivery=document&adapter=/presenter-adapter.js
```

Keep the announced origin and authentication fragment. Bootstrap removes the
fragment after consuming it; do not publish capability-bearing URLs or evidence.
For acceptance only, add `&inspect=1` to expose
`window.__bmuxDocumentInspection()`. Its result contains `generation`, `revision`,
`publication_revision`, `viewport`, and `frame`: committed public Canvas metadata,
not private presenter state or an uncommitted candidate.

Delivery representation is selected at host startup with
`BMUX_PRESENTER_WEB_DELIVERY=document` (semantic) or
`BMUX_PRESENTER_WEB_DELIVERY=frame` (legacy fixed frame). A document-only host
cannot synthesize a frame feed merely because a browser changes its query.
Fixed-frame consumers explicitly select `?delivery=frame` on a frame-mode host.
The two modes are not
interchangeable: fixed-frame delivery retains producer geometry, while document
mode composes public semantic content locally. Existing fixed-frame acceptance
checks, including first-viewer viewport authority and observer leases, remain in
`scripts/presenter-web-acceptance.py` and run explicitly in frame mode.

The generic feed is `GET /document?version=1`. A complete publication is:

```text
{version: 1, publication_revision, generation, revision,
 capability: "bmux.presenter.audience.v1",
 content: [JSON UTF-8 bytes],
 resources: [{id, mime_type, length}]}
```

Revision values are decimal strings, not JavaScript floating-point counters.
Resources are opaque SHA-256 identities served at `/resource/<sha>` with the
announced length and MIME type. Feed publication and its manifest are one atomic
replacement. The generic web host does not interpret presenter content.

## Composition and ownership

The presenter adapter uses WASM `AudienceRenderer`, shared presenter components,
and the canonical `CanvasRenderer`. Browser width, height, scroll, and focus are
local view inputs. Prose reflows through shared components; native code, images,
and Mermaid output must not silently become diagnostic text. Shared navigation
and reveal state come from the publisher, not from a viewer's layout.

Custom/live output is a fixed retained raster embedded in that component tree.
Cells and images use the same committed transforms and clipping. Resizing an
embedding does not reflow its producer, start or stop execution, change approved
source, or transfer lifecycle ownership. See [live components](live-components.md).
Browser wheel input scrolls the local audience only; it is not producer input.

Audience publications contain only revealed public content, approved image
resources, and retained public surfaces. Notes, private custom configuration,
unrevealed blocks, and unrevealed resources must not be recoverable through the
feed, resource endpoints, or committed inspection metadata. Raster regions do not
invent logical selection or semantics: partial/unavailable metadata is explicit.

## Transactional failure and resource reuse

Preparing a candidate must not alter the last committed cells, images, selection,
hits, focus, or semantics. Invalid content, stale identity/revision, failed resource
fetch/decode, and obsolete viewport preparation retain that scene. A complete
successful repair commits output and metadata together. A local resize recomposes
from retained content without publishing a new shared navigation revision.

Successfully fetched image resources are cached by immutable identity and reused
on resize and navigation while retained within the cache budget. An aborted or
failed fetch does not become a valid cache entry. Acceptance counts actual browser
fetches rather than relying on HTTP cache hits.

## Coordinator-owned acceptance

No builds, downloads, runtime launches, or browser launches are implicit in this
document. The coordinator supplies an explicit built executable, plugin directory,
asset directory, Chromium executable, isolated state parent, and owned LAN address
when allocating a test slot:

```sh
python3 scripts/presenter-web-acceptance.py \
  --binary /absolute/bmux --plugins /absolute/plugins \
  --assets /absolute/web-assets --browser /absolute/chromium \
  --state-parent /owned/private-state --lan-address OWNED_NON_LOOPBACK_IPV4
```

The harness sets frame mode for legacy play/host/config/network launches and
document mode for semantic launches. It requires presenter adapter/JS/WASM assets,
and records their hashes along with the native binary, plugins, browser, and
embedded WASM Mermaid renderer. No helper executable is needed.

The harness retains private artifact hashes and per-case results. It runs the
existing fixed-frame suite plus a document-mode case with simultaneous narrow and
wide audiences, native prose/code/image and custom-raster fixtures, local wheel
scroll/focus/resize isolation, shared navigation, privacy and partial metadata,
source-linked wrapped-row geometry, PNG aspect/centering and placement below
reflowed prose, real local Canvas focus, resource reuse, and recovery from invalid content, stale revisions, and failed
resources. Fault injection wraps one browser's fetch only: the real product feed,
adapter, WASM components, and Canvas commit path remain in use.

Source-ready is not a PASS. Final validation and runtime evidence are coordinator
owned. The new document case checks the native presenter's process survives
resize; it does **not** prove a running playbook attempt retains its execution
identity. That lifecycle scenario still requires an explicitly approved live
fixture and coordinator-owned execution slot. The fixture checks code and PNG
images. A separate Mermaid case requires loader substitution to a mapped prepared
PNG and actual colored Canvas output at two widths; no browser Mermaid execution
or diagnostic fallback is accepted.

## Metadata and interaction scope

Browser frames use JSON wire version 3 and binary `BMWP\x03`. The HTTP
`/presentation?version=1` and `/document?version=1` negotiation remains version 1.
Rebuild matching native and WASM consumers together; older frame schemas are
rejected rather than interpreted with guessed metadata semantics.

Shared Markdown composition publishes source-linked selection geometry and
semantic regions. Source maps include only IDs referenced by clipped candidate
fragments; embedded rasters declare regional metadata unavailability explicitly.
Native prepared code retains authored UTF-8 provenance at grapheme boundaries.
Painting and interaction metadata use the same committed transforms and clipping.

Browser acceptance covers local scrolling and Canvas DOM focus, not interactive
BMUX focus-target traversal or pointer selection/copy. Metadata availability must
not be described as implementing those interactions. The execution-identity
scenario described above is not covered by the current browser fixture.

## Validation

Run repository-required Rust validation in the pinned development shell, rebuild
bundled plugins and matching browser assets, then run the explicit-artifact browser
harness above. All nine browser cases cover fixed-frame play/hosting, responsive
audiences, prepared Mermaid images, configuration precedence, LAN and IPv6
access, authentication recovery, and owned-process cleanup. Failed-document
checks capture the last committed scene immediately before fault delivery so a
valid in-flight publication is not mistaken for acceptance of a failed candidate.
Retained evidence is private and may contain capabilities; do not publish it
without redaction.
