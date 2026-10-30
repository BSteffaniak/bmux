# bterm_native_renderer

Window-owned GPU output for BMUX's retained `PreparedPresentation`. This crate
projects existing cell/image geometry into physical pixels; it does not own
layout, resource lifecycle, hit testing, selection, or interaction authority.
Wgpu and font backend types remain private.

Create `NativeRenderer` from the platform's opaque owned `SurfaceTarget` and
`RendererOptions`. Use `cell_size()` to size the presentation projection (not
the
PTY), resize on drawable-size changes, and update font metrics on DPI changes.
`RendererOptions` remains source-compatible. Configure typography separately
with
`NativeRenderer::set_typography(text::RendererTypography { ..Default::default()
})`;
`font_size` and `letter_spacing` are logical pixels, while `line_height`
multiplies
the selected face's natural line height. Defaults are size 16, natural line
height,
zero spacing and bundled JetBrains Mono. `set_scale_factor` applies DPI
separately.
Both setters delegate to `set_typography_and_scale(typography, scale)`, which
validates/resolves the requested pair before mutation. Use the combined method
on display changes: logical128/DPI2 → logical256/DPI1 is valid even though
either
individual setter could reject its intermediate combination. Unsupported numeric
values return errors without changing fonts, options, geometry, caches or
damage.
Font sizes must resolve to 1–256 physical pixels, line height is 0.25–8,
spacing
is −128–128 logical pixels, and the resulting cell dimensions must be
positive.
Family names are nonblank, at most 256 bytes each; at most 32 fallbacks are
accepted.

`geometry()` returns fractional physical cell dimensions, baseline and reusable
`cell_origin`/`physical_to_cell` transforms. Inverse coordinates are continuous:
callers apply their committed origin, clip and bounds before flooring to cells.
Snapshot geometry with the successfully displayed presentation, not merely when
a
setter succeeds. Pending metrics are not new interaction or scene authority.
No integer ceil is applied to terminal cells, images, cursors or interaction
bounds.

Requested primary/fallback families precede the bundled safety net, followed by
installed fallbacks. With no primary, the bundled face stays primary. Missing
names
do not fail or silently rewrite configuration. Faces resolve by family/coverage
and closest bold/italic style; color emoji are preferred for emoji presentation.
Installed-face ties use family, PostScript name, path and face index, not
enumeration
order. Family changes reload bounded faces; size-only animation reuses them.

Glyph outlines are antialiased at the effective size, not enlarged bitmaps.
Raster
size keys use 1/64 physical pixels; baseline phases use 1/8 pixel with coverage
baked
into outlines and integer bitmap quads. Allocation geometry stays continuous.
The LRU retains at most 2048 cells / 8 MiB including index, Arc, keys and
allocation
capacities, independently of how many animated sizes/phases are encountered.
Caller-held raster Arcs and in-flight frames remain caller-bounded.

## Integration bounds and geometry

`text::{MIN_RASTER_PIXEL_SIZE, MAX_RASTER_PIXEL_SIZE, MAX_LOGICAL_FONT_SIZE}`
exports the supported limits (1, 256, 256). Keep configuration/base size and
saved
zoom unchanged. For finite positive DPI, clamp the **effective logical size** to
`[max(1, MIN_RASTER_PIXEL_SIZE / dpi),
min(MAX_LOGICAL_FONT_SIZE, MAX_RASTER_PIXEL_SIZE / dpi)]` before passing
typography.
Reject an empty interval/unsupported DPI instead of weakening raster bounds.
For base 128 × zoom 3 × DPI 2, use effective logical size 128 (physical 256),
not logical 384 (physical 768). Account for the final f32 multiplication when
checking the physical bound. The renderer intentionally returns an error rather
than silently applying a different size behind the application's geometry.

Use committed `geometry().physical_to_cell_f64(point)` for pointer inversion and
floor only after committed allocation/clip checks. `cell_rect_f64` projects the
same cell dimensions for accessibility/IME; divide physical rectangles by the
committed DPI only when a platform API requires logical coordinates. Paint uses
those same f32 cell dimensions and narrows final GPU coordinates to f32. Neither
cell boundaries nor pointer coordinates are rounded to whole pixels. Only glyph
coverage uses deliberate 1/8-pixel phase quantization (maximum 1/16-pixel
baseline
adjustment); this does not alter allocation, selection, images, cursor or IME
bounds.

## Coordinator quality/performance validation

Run in the Nix/direnv development shell after integration (this lane does not
run
cargo concurrently with coordinator validation):

```sh
cargo test -p bterm_native_renderer
cargo run --release -p bterm_native_renderer --example typography_workload
cargo run --release -p bterm_native_renderer --example native_render_workloads -- --window fractional-zoom
cargo run --release -p bterm_native_renderer --example native_render_workloads -- --window warm-text
```

The CPU workload is finite/headless (2304 frames per case). CSV reports setup,
first-frame, p50/p95 raster wall time and peak cache entries/bytes for warm
reuse
versus fractional-size churn; bounds are asserted. Quality tests cover coverage,
vertical baseline direction, combining-source boundaries, size/phase cache
bounds,
fractional geometry, pointer-floor boundaries and deterministic family fallback.
The finite native fractional-zoom case renders an ascending/descending size
sweep
with line height 1.15 and spacing 0.25, retaining the existing per-stage CSV
telemetry.
It requires an available desktop/GPU and gives visual/CPU evidence, not
compositor
presentation or input-latency measurements. No benchmark numbers or visual
quality
pass are claimed until the coordinator runs these commands.

Render only inside the frontend presentation authority's synchronous output
callback. `PresentReport` reports successful surface output, never permission to
commit a stale delivery. Minimized, lost, outdated, invalid, or
capacity-exceeded
output returns an error and must not commit interaction metadata.

The shader alpha-blends batched physical quads. Cell backgrounds precede shaped
text and decorations; retained image placements and the cursor follow. The
swapchain is conservatively fully redrawn because its contents are not retained.
Image allocations and clips come directly from the reconciled image scene.
No terminal encoding, browser frame, listener, or secondary layout is involved.

A fixed 4096-square RGBA atlas is reclaimed each frame; repeated cell glyphs
share
uploads within a frame. CPU text/font caches and image decoding are bounded.
Oversized frames fail explicitly rather than partially succeed. Dropping the
renderer releases its GPU surface, atlas, buffers, font state and window token.
Surface loss/outdating triggers reconfiguration but still fails that
transaction;
the caller may retry a currently valid delivery. Desktop window/display testing
is separate from pure raster, image validation and color tests.
