# bterm_native_renderer

Window-owned GPU output for BMUX's retained `PreparedPresentation`. This crate
projects existing cell/image geometry into physical pixels; it does not own
layout, resource lifecycle, hit testing, selection, or interaction authority.
Wgpu and font backend types remain private.

Create `NativeRenderer` from the platform's opaque owned `SurfaceTarget` and
`RendererOptions`. Use `cell_size()` to size the presentation projection (not
the
PTY), resize on drawable-size changes, and update font metrics on DPI changes.
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
