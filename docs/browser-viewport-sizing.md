# Browser viewport sizing

Document delivery gives a renderer adapter a mount element:
`createDocumentRenderer({mount})`. The adapter owns its output elements and their
cleanup. The host does not require a Canvas, cell metrics, or raster buffers.

Each `prepare(document, viewport, options)` receives client-local CSS dimensions
(`width`, `height`) and requested `device_pixel_ratio`. CSS dimensions describe
available layout space; device scale is a rendering hint, not a layout multiplier.
Adapters retain the existing prepare/commit/discard transaction: failed or
superseded preparation must not advance displayed output or interaction authority.

The presenter Canvas adapter derives its logical grid from CSS dimensions and
uses the existing Rust compositor for wrapping, clipping and view-local scrolling.
It separately chooses a bounded raster density. Its visible CSS size must never
inherit its high-DPI backing dimensions. Pixel data, backing size, display size
and committed scene authority advance together.

Fixed-frame delivery preserves producer geometry and proportionally contains the
frame in the available browser area. Fitting does not confer producer resize
authority; the existing explicit controller lease still governs resize requests.
Dense document content remains accessible through authoritative internal scrolling,
not accidental page overflow or silently reduced typography.

Future WebGL and HTML adapters use the same CSS viewport and transaction contract.
Drawing-buffer limits, Canvas cell metrics and pixel budgets belong to raster
adapters, not the document transport or mount interface.
