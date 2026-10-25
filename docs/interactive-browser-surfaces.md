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

## Runnable custom component

`packages/web-renderer/examples/custom_component.rs` implements a caller-owned
counter using the public `Component` trait, retained `LocalSurface`, committed
focus/hit routing, and logical source-text selection. It has no application-domain
or terminal dependencies. Its native smoke entry point is runnable directly:

```sh
bash scripts/dev.sh cargo run -p bmux_web_renderer --example custom_component
```

For the interactive browser example, build the WASM binary with the locked Rust
shell, then generate its browser bindings with **wasm-bindgen-cli 0.2.126** (the
version must match the workspace's wasm-bindgen dependency):

```sh
# Optional local tool setup; never use a mismatched CLI or a global install.
bash scripts/dev.sh cargo install --locked --root target/web-tools \
  --version 0.2.126 wasm-bindgen-cli
bash scripts/dev.sh bash packages/web-renderer/tests/build-custom-component.sh
BMUX_CUSTOM_BROWSER_TEST=1 CHROMIUM_BIN=/absolute/path/to/chromium \
  node --test packages/web-renderer/tests/custom-component-browser.test.mjs
```

The build helper verifies the CLI version and generates bindings in ignored
`target/custom-component-web/pkg`. `BMUX_WASM_BINDGEN` can select another exact
0.2.126 executable. For manual browsing, generate bindings into `examples/pkg`
instead (build output only), then serve the source directory:

```sh
bash scripts/dev.sh bash -c 'target/web-tools/bin/wasm-bindgen --target web \
  --out-dir packages/web-renderer/examples/pkg \
  "$CARGO_TARGET_DIR/wasm32-unknown-unknown/debug/examples/custom_component.wasm"'
python3 -m http.server 8000 --directory packages/web-renderer
```

Open `http://localhost:8000/examples/custom-component.html`. Each embed has its own
Rust state, Canvas, input bridge, selection controller, and bounded FIFO (64
pending events). Click/Enter increments; click the source line and type to append
UTF-8 text or Backspace to remove the last scalar. This deliberately small editor
is not a full grapheme-aware text control. Dragging selects logical source text,
and the explicit copy button calls the supplied clipboard capability. Resize,
hide/show, dispose, and remount exercise lifecycle transitions. The host serializes mutable
WASM calls, coalesces resize requests, cancels pointer input queued against a
superseded scene, and frees WASM only after an in-flight presentation settles.
Queue exhaustion disconnects visibly rather than dropping a submission silently.
Generated `examples/pkg` files are build output, not source.

## Embedding lifecycle and budgets

`CanvasRenderer::from_canvas` (`fromCanvas` in generated JS) accepts a caller-owned
Canvas element, including one in a shadow root, without document-global IDs. The
existing ID constructor and read-only viewers retain their behavior. Metrics are
physical pixels; the host owns CSS size, cell size, and bounded DPR policy. Empty
viewports are valid committed scenes and must not blit a zero-sized Canvas source.
A resize notification alone never authorizes new hit geometry.

Mount at most one interactive bridge per Canvas; dispose it before remounting.
Competing pointers cannot replace the single shared mouse gesture. Releases
outside the caller's committed mapping cancel instead of activating the last
in-bounds target. Zero-size transitions cancel capture and suppress point mapping.
Disposal detaches listeners, observers, pointer capture, and semantic DOM before
notifying application callbacks, so even a throwing callback cannot retain those
resources. Disposal is idempotent; pending application effects remain caller-owned.

Semantic replacement validates IDs and text before changing the DOM (4096 regions,
512 UTF-16 units per ID, 65536 per string, 1048576 aggregate units). It projects
explicit committed accessibility only, suppresses sensitive text, and grants no
semantic-action or clipboard authority. A native screen-reader acceptance audit is
still required; this is not a claim of full accessibility or mobile IME parity.

## Validation

Run Rust commands with the repository's Nix-selected toolchain:

```sh
bash scripts/dev.sh cargo test -p bmux_web_renderer
bash scripts/dev.sh cargo check -p bmux_web_renderer --features wasm --target wasm32-unknown-unknown
node --test packages/web-renderer/tests/interactive-surface.test.mjs
BMUX_INTERACTIVE_BROWSER_TEST=1 CHROMIUM_BIN=/absolute/path/to/chromium \
  node --test packages/web-renderer/tests/interactive-surface.test.mjs
```

The standalone harness builds actual Rust/WASM and drives Chromium through its
DevTools input path: independent counters, native UTF-8 insertion/Backspace,
Canvas pixels, logical source selection and explicit copy callback, zero/tiny
resize recovery, DPR 2, disposal/remount, and denied clipboard capability. The
injected test access exposes only the example's existing caller-owned controls;
it does not replace WASM, Canvas, routing, or application state. Clipboard denial
is an injected capability rejection, not an OS permission-dialog acceptance test.

The real-browser bridge case covers native text editing, composition event
collection, independent embeds, resize, and cleanup. It does not itself prove
application/backend integration, complete native IME/mobile parity, or full
accessibility. Full repository validation requirements still apply.
