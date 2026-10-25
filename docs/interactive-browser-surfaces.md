# Interactive browser-local BMUX surfaces

Interactive surfaces run application composition and updates locally in
Rust/WASM.
They are distinct from the read-only attach/presenter viewers: they do not
consume
terminal bytes, create PTYs, attach to a daemon, or receive authority from a
decoded
presentation frame. Applications provide their own data transport and platform
capabilities.

## Ownership and presentation

`bmux_web_renderer::LocalSurface` owns reconstructible presentation and
interaction
caches. The caller owns application/control state, stable component identities,
accepted complete plugin presentation snapshots, subscriptions, and effects.
Painting must not invoke plugins or perform application updates.

Compose through BMUX components and the retained `PaintCx` boundary. Use
`LocalSurface::present` for a transactional output callback, or
`present_canvas` for direct browser-local Canvas delivery. The latter passes a
prepared presentation directly to `CanvasRenderer`, without a JSON/binary
transport roundtrip. Metadata-staging variants support caller-owned modal chains
and source selection data; admission limits apply before output.

Only successful presentation installs the interaction scene. Route input with
`LocalSurface::route`; never hit-test independently in JavaScript. Resize does
not
make undisplayed geometry authoritative. Cancel transient pointer state on blur,
lost capture, disposal, and pointer cancellation. Output adapters must publish
pixels transactionally; failed output leaves committed interaction metadata
intact.

## Browser bridge

`packages/web-renderer/browser/interactive-surface.js` exports
`mountInteractiveSurface`. The application supplies a container, Canvas,
`onInput`, authoritative `mapPoint`, resize/capability callbacks, and optional
explicit clipboard effects. The bridge does not implement a widget or
application
keymap. It owns browser listeners, a native text-entry bridge, pointer capture,
IME collection, and resize/DPR observation. Dispose it when the surface is
removed.
Multiple surfaces may coexist without global keyboard ownership.

Text arrives after native editing/composition, separately from shortcut keydown.
Do not also insert printable keydown text. `BrowserInput::decode` bounds encoded
input before allocation; `normalize` bounds text and maps coordinates through
the
caller. Current shared input represents committed text and paste as
`Event::Paste`.
Wheel magnitude is reduced to at most one directional event per axis;
applications
must not assume pixel-precise wheel deltas survive normalization. Keyboard meta
is
retained, while shared mouse primitives do not represent meta.

Browser-reserved shortcuts, trusted clipboard permissions, native IME event
ordering, accessibility, and mobile keyboard behavior require browser-specific
acceptance. A passing mocked event test does not establish that acceptance.
Semantics must project committed BMUX metadata; do not build another layout or
hit
scene in the DOM.

## Application integration

Browser application hosts serialize mutable WASM calls, including asynchronous
Canvas preparation. They own cancellation, bounded event admission, network
resynchronization, authentication, action retry identity, and resource budgets.
A latest-only presentation feed is not a reliable submission queue. Reconnecting
must not invent a second submission identity after an ambiguous result.

No shell, filesystem, backend mutation, clipboard, or navigation authority is
implied by mounting a surface. Capability access is explicitly supplied by the
embedding application. Backend authorization remains authoritative.

## Validation

Run Rust commands with the repository's Nix-selected toolchain:

```sh
bash scripts/dev.sh cargo test -p bmux_web_renderer
bash scripts/dev.sh cargo check -p bmux_web_renderer --features wasm --target wasm32-unknown-unknown
node --test packages/web-renderer/tests/interactive-surface.test.mjs
BMUX_INTERACTIVE_BROWSER_TEST=1 CHROMIUM_BIN=/absolute/path/to/chromium \
  node --test packages/web-renderer/tests/interactive-surface.test.mjs
```

The real-browser bridge case covers native text editing, composition event
collection, independent embeds, resize, and cleanup. It does not itself prove
application/backend integration, complete native IME/mobile parity, or full
accessibility. Full repository validation requirements still apply.
