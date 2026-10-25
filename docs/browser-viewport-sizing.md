# Browser viewport sizing

Document delivery gives a renderer adapter a mount element:
`createDocumentRenderer({mount})`. The adapter owns its output elements and
their
cleanup. The host does not require a Canvas, cell metrics, or raster buffers.

Each `prepare(document, viewport, options)` receives client-local CSS dimensions
(`width`, `height`) and requested `device_pixel_ratio`. CSS dimensions describe
available layout space; device scale is a rendering hint, not a layout
multiplier.
Adapters retain the existing prepare/commit/discard transaction: failed or
superseded preparation must not advance displayed output or interaction
authority.

The presenter Canvas adapter derives its logical grid from CSS dimensions and
uses the existing Rust compositor for wrapping, clipping and view-local
scrolling.
It separately chooses a bounded raster density. Its visible CSS size must never
inherit its high-DPI backing dimensions. Pixel data, backing size, display size
and committed scene authority advance together.

Fixed-frame delivery preserves producer geometry and proportionally contains the
frame in the available browser area. Fitting does not confer producer resize
authority; the existing explicit controller lease still governs resize requests.
Dense document content remains accessible through authoritative internal
scrolling,
not accidental page overflow or silently reduced typography.

## Embedded playbooks

Open playbooks publish their terminal scene separately from audience chrome. The
terminal retains its authored rows, columns, images and cursor; fitting never
resizes execution, reflows cells, or scales the title, status or progress bar.
Only the terminal surface shrinks uniformly, with its container reserving the
rounded-up scaled height rather than the original terminal height.

The presenter computes destination and clipping in the shared Rust layout/paint
path. Renderer adapters consume that pending projection and composite it before
committing the surrounding scene. The Canvas adapter uses lossless `frame_json`
for nested wire revisions and bounded, temporary raster stores. Render failure
or
supersession retains the previous terminal pixels and chrome together.

Audience terminal surfaces remain inert: they do not acquire terminal input,
focus or synthetic selection targets when scaled. Their selection and semantic
availability is explicitly unavailable. Presenter controls and the controlling
terminal's geometry remain unchanged.

Future WebGL and HTML adapters use the same CSS viewport and transaction
contract.
Drawing-buffer limits, Canvas cell metrics and pixel budgets belong to raster
adapters, not the document transport or mount interface.
