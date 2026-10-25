
## Public live viewer

`/live` is an output-only audience page backed by the same Rust/WASM Canvas
renderer as the local browser. It receives versioned binary patches over a
persistent
WebSocket from `https://live.bmux.dev`; it never claims a local view, sends
input, or
reads a local bootstrap token. Paused, offline, incompatible, expired, and
replaced-epoch output is removed, including detached pending render targets.

### Build and static deployment

Before compiling the docs application or generating the static site, run:

```sh
bash scripts/dev.sh bash scripts/build-web-renderer.sh
```

The script builds the renderer in the Nix-selected target directory, writes the
native asset bundle to `target/web-assets/` (or `BMUX_WEB_ASSET_OUTPUT`), and
stages
these audience files under `public/live-assets/<bundle-sha256>/`:

- `canvas-viewer.js`
- `live-viewer.js`
- `live-page.js`
- `bmux_web_renderer.js`
- `bmux_web_renderer_bg.wasm`
- `manifest.json` (protocol, renderer, bundle identity, file sizes and SHA256s)

The docs UI build reads `public/live-assets/current-version` and includes the
immutable module URL on **every** shell entry route. The generated site's
registered `live-assets` directory route must be copied to the deployment root,
not served only under `/public`. `/live` is in the central site page registry
and
is included in normal static route generation. Serve `.wasm` as
`application/wasm` and JS as JavaScript. Configure the relay origin with
`BMUX_LIVE_ENDPOINT_ORIGIN` during the web build (default
`https://live.bmux.dev`).
The relay admits exact configured website origins. The unversioned fallback
intentionally fails closed when the
explicit web build has not run; ordinary native/doc builds do not install WASM
tooling or contact the relay. Do not include `bootstrap.js` or its local token
flow on the public page. The release packager also includes all three new module
names; native web-host's asset allowlist must match.

### Reuse and cleanup

Import `mountLiveViewer` from the immutable bundle's `live-viewer.js` and pass
`{container, status, apiBase, assetBase}`. `container` and `status` are explicit
DOM elements, `apiBase` is the HTTPS relay origin, and `assetBase`
is the directory containing the renderer JS/WASM. Await the returned handle and
call `dispose()` on unmount. `mountCanvasViewer` independently supplies
`present(bytes, admissionPredicate)`, `clear()`, and `dispose()` for other
read-only transports. It exposes no keyboard, viewport, or navigation controls.

### Focused tests and real-browser acceptance

```sh
node --test packages/web-renderer/tests/live-viewer.test.mjs
```

For coordinator-owned browser validation, use an isolated relay/port and the
real `snapshot --binary` example frame. Load a non-live docs route, navigate to
`/live`, and verify actual text/image Canvas pixels with the shipped WASM. Check
direct `/live` entry and back/forward SPA navigation. Delay incoming frame
processing and renderer/image decode; pause, stop/offline, replace epoch,
and leave `/live` during each delay. The old Canvas must be removed immediately
and late completion must not reconnect it. Resume with a fresh full frame and
verify rendering recovers. Disconnect the relay and expire its lease; output
must clear rather than freeze. Navigate repeatedly and confirm only one viewer
WebSocket exists per mounted viewer, it closes after unmount, and no `/view`,
`/viewport`, input endpoint, Authorization
header, or local token bootstrap is used. This requires real browser evidence;
the Node mocks alone do not establish Canvas pixel correctness.
