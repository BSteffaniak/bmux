# Browser rendering

BMUX shares retained presentation primitives between terminal output and a Rust/WASM browser renderer. Canvas 2D is the initial browser backend; `BrowserBackend` keeps output replaceable without exposing Canvas, DOM, or GPU objects in the shared contract. Attach and presenter viewers remain read-only. Applications can opt into [interactive browser-local surfaces](interactive-browser-surfaces.md) using the same compositor and a separate browser input bridge; this does not make attach viewers remotely interactive.

## Attach locally

Browser output is opt-in; ordinary terminal attachment needs no browser assets or
network setup:

```sh
bmux attach "<SESSION_UUID>" --web
bmux attach "<SESSION_UUID>" --web-only
```

`--web` mirrors an interactive terminal. `--web-only` enables browser output
without a terminal. Neither requires `--web-assets`. With no asset override,
BMUX lazily resolves compatible browser assets, reusing its durable cache or
setting it up on first use. A compatible cached bundle supports offline use;
a cache miss needs setup to succeed. Setup errors stop web activation before
opening a listener, entering raw mode, or attaching. Terminal-only commands and
network-only flags never trigger asset setup.

For attach and presenter entrypoints, cache authority is explicit `BMUX_CACHE_DIR`
first, otherwise the selected BMUX state root's `cache` directory. The resolver
adds its `web-assets` namespace underneath that durable root. Attach honors
slot-selected paths and named-runtime/sandbox path selection; presenter uses its
native caller's selected state root. No global environment mutation is required,
and the ephemeral socket/runtime directory is never used as the asset cache.
The context-free resolver separately falls back from `BMUX_CACHE_DIR` to
`XDG_CACHE_HOME` to `HOME`, rejecting unresolved isolated selections rather than
crossing into ordinary cache.
First use from a source checkout builds with the pinned generator and caches the
result. Released binaries resolve a pinned-tag bundle manifest instead. Older
releases without a published compatible bundle cannot silently use another
version: follow the setup error, upgrade to a bundle-bearing release, or build
matching assets and pass `--web-assets DIR`. Offline first use needs a populated
compatible cache or an explicit matching asset directory.

### Advanced: locally built assets

`--web-assets DIR` remains an explicit advanced override and also enables mirror
mode. Asset selection is CLI `--web-assets` → `BMUX_WEB_ASSETS` → implicit setup;
an explicit path is validated, not silently replaced or installed over. Merely
setting `BMUX_WEB_ASSETS` or network configuration does not enable web output.
For source development, build matching native and WASM artifacts using the
locked Nix shell:

```sh
bash scripts/dev.sh cargo install wasm-bindgen-cli --version 0.2.126 --locked --root "$PWD/target/web-tools"
bash scripts/dev.sh bash scripts/build-web-renderer.sh
bash scripts/dev.sh cargo run -p bmux_cli --bin bmux -- attach "<SESSION_UUID>" --web-assets "$PWD/target/web-assets" --web-only
```

Replace `<SESSION_UUID>` with an ID from `bmux list-sessions --json`. `--web-only` opens an independent output-only attachment: it does not require terminal input, raw mode, or ANSI output. Its initial viewport is 80×24 until the first browser supplies measured geometry. Stop the owned attachment with Ctrl-C; disconnecting its browser does not terminate the server-owned process.

An independent attachment does not follow another client's focused pane, zoom state, or selected tab. To follow your laptop view, identify that terminal client with `bmux list-clients --json`, then use its **client UUID** (not the session UUID):

```sh
bmux attach --follow "<LAPTOP_CLIENT_UUID>" --global --web-only
```

`--global` keeps following when that client switches sessions. Choose the client explicitly; BMUX does not guess among attached clients.

Use `--web` instead of `--web-only` to mirror an interactive terminal attachment. In mirror mode the terminal owns viewport dimensions; browser resizing only changes local pixel metrics, not terminal layout. Native provider-only attachment is explicitly unsupported by this entry point.

The default browser URL is **<http://127.0.0.1:8080/>**: fixed loopback port,
no token, and ordinary reload works. Any local user/process can read this output.
Host/Origin and Fetch Metadata checks still reject cross-site access; viewport
control still requires the leased controller. No remote interface is exposed.

Override settings in precedence order **CLI → environment → config → defaults**:

```sh
# Opt into the original ephemeral capability-URL behavior:
bmux attach "<SESSION_UUID>" --web-assets "$PWD/target/web-assets" --web-only --web-port 0 --web-auth token
# Or choose another stable loopback port:
BMUX_WEB_PORT=8081 BMUX_WEB_AUTH=none bmux attach "<SESSION_UUID>" --web-assets "$PWD/target/web-assets" --web-only
```

```toml
[web]
port = 8080 # 0 selects a random available port
auth = "none" # "token" requires the printed capability URL
```

An occupied fixed port is an explicit startup error, never a silent switch to
another port. Token mode prints a capability in the URL fragment. Open it locally;
do not share or record it. The browser removes the fragment before network
requests and retains credentials only in memory. Reloading in token mode requires
reopening the capability URL. The browser cannot send terminal input or invoke
mutating plugin actions. Viewport changes are allowed only for the controlling
browser of an independent attachment.

## Native LAN access

Loopback remains the default. Bind your LAN IP (no token required by default):

```sh
bmux attach "<SESSION_UUID>" --web-assets "$PWD/target/web-assets" --web-only \
  --web-host 192.168.1.188
```

Or listen on all IPv4 interfaces—no public-host setting needed:

```sh
bmux attach "<SESSION_UUID>" --web-assets "$PWD/target/web-assets" --web-only \
  --web-host 0.0.0.0
```

With a wildcard bind, the displayed URL uses loopback for local use. On another
computer, use the serving computer's LAN IP or DNS name, e.g.
`http://192.168.1.188:8080/` or `http://bmux.lan:8080/`. Never browse to `0.0.0.0`.
Refresh works normally. `--web-public-host 192.168.1.188` optionally changes the
printed URL only; it neither restricts hosts nor changes the listening interface.
Authentication is independent of the bind address: add `--web-auth token` to opt
in. With token mode, open the full printed capability URL including `#token=...`.
Deployment owners can instead provide access controls at their network boundary.

Equivalent configuration:

```toml
[web]
host = "0.0.0.0"
port = 8080
auth = "none" # default on any bind address; "token" opts in
```

`BMUX_WEB_HOST` and `BMUX_WEB_PUBLIC_HOST` are the environment equivalents.
All fields independently honor CLI → environment → config → defaults precedence.
Bind and URL-display hosts must be literal IPv4/IPv6 addresses. Browser URLs can
use DNS aliases or any reachable interface. For IPv6 use `--web-host ::`; displayed
URLs bracket IPv6. Scoped link-local bind addresses are unsupported.

By default any syntactically valid HTTP Host is accepted. A supplied Origin must
match `http://<request Host>`; viewport mutations require that same Origin.
Cross-site Fetch Metadata is still rejected. No forwarded host/proto headers are
trusted. For optional exact host restrictions use:

```sh
--web-allowed-hosts bmux.lan:8080,192.168.1.188:8080
```

Or configure `allowed_hosts = ["bmux.lan:8080", "192.168.1.188:8080"]` under
`[web]`, or set comma-separated `BMUX_WEB_ALLOWED_HOSTS`. The list includes ports;
repeat the CLI flag to add entries. An empty list means unrestricted hosts (an
empty environment value explicitly clears configured restrictions). This is
independent of `public_host` and token authentication. DNS-rebinding host
restrictions are opt-in, not implicit in the bind address.

**Deployment policy:** token authentication is opt-in for every bind address.
With `auth = "none"`, any client that can reach the listener and satisfy the
Host/Origin policy can read this presentation and acquire its leased viewport
controls. External access controls are deployment-owned. HTTP is unencrypted;
tokens restrict access but do not encrypt output or credentials. Native TLS and
HTTPS reverse-proxy deployment are not provided by these options. Neither mode
grants terminal input, plugin actions, or broader BMUX permissions. Token-mode
refresh requires reopening the capability URL.

## Viewport ownership and lifecycle

- The first browser claims a separate random controller lease; additional viewers mirror the same composed view.
- Lease renewal occurs periodically; expiry, generation changes, explicit release, or source loss revoke resize authority. An old owner's queued request cannot become a new owner's request.
- Font, container and device-pixel-ratio changes update Canvas metrics. Bounded cell dimensions are sent through an authenticated latest-only channel, not a terminal input endpoint.
- Native sizing uses the existing attachment/PTY policy and authoritative plugin layout; there is no browser-specific competing layout authority.
- Temporary transport loss repairs from a complete snapshot. Source closure shuts down hosting rather than retaining a healthy-looking stale frame. The attachment host remains alive while an independent viewer disconnects; explicit shutdown closes it.

The host defaults to IPv4 loopback port 8080 and supports IPv4/IPv6 listeners with opt-in token authentication. It restricts assets to an explicit allowlist, validates configured Host/Origin and optional capability headers, and bounds bodies, connections, requests and snapshots. Styles are external same-origin assets compatible with the strict CSP.

## Presentation admission and recovery

Wildcard startup distinguishes the listener (`0.0.0.0` / `::`), a local-use URL,
and LAN access instructions. Loopback in the local URL does not mean the listener
is loopback-only.

Browser capture admits visible operations rather than terminal control-command
count. Adjacent same-style text is coalesced, and captured image placements share
owned resources instead of deep-cloning them on every capture read. Large raw
pixel resources are losslessly PNG-compacted before placement expansion. Decoded
image admission supports up to 16,777,216 pixels, including 4K images; encoded
resource and aggregate memory limits still apply. Incompressible or heavily
fragmented resources remain explicitly bounded rather than being silently dropped.

Browser-enabled frame rejection keeps the attachment and HTTP listener alive. It
retains the last complete frame, compositor and interaction metadata, reports the
specific capture/admission stage and limits, and retries after a new source or
viewport event. Identical rejection reports are deduplicated; the render loop does
not spin on unchanged rejected content. `/view` exposes bounded
`presentation_status`, independent of the committed output generation. The browser
shows a paused status on its next claim/long-poll refresh without clearing Canvas.
A successful complete publication clears the status. Terminal write failures and
source closure retain their existing error/lifetime semantics.

Regression coverage includes 600×300 per-cell command capture, sustained real PTY
updates, 4K image rendering, oversized-image rejection/recovery, synchronized
updates, and failed-publication rollback. This does not identify which branch
caused a historical failure without its diagnostic evidence.

## Shared presentation boundaries

- `bmux_tui::presentation` prepares owned cells, cursor, images and metadata. Only successful output commits retained authority.
- Terminal and headless frontends consume the same lowering/composition path. Neutral composition does not encode terminal bytes. Mirroring repairs complete browser snapshots from retained cells without forcing a terminal full repaint solely for capture.
- Plugins publish owner-scoped retained presentation. Legacy extension adapters cache producer intent outside frame composition; they are compatibility adapters, not a replacement ownership model.
- Synchronous browser serialization/publication failures prevent headless presentation commit. Browser delivery has its own successful-output boundary; a failed Canvas presentation retains previously committed metadata.
- `bmux_web_renderer` validates bounded versioned snapshots and backend capabilities, stages Canvas output/resources, and commits metadata only after successful output. It has no native client, daemon or plugin-loader dependency.
- `bmux_web_host` treats snapshots as opaque immutable bytes. Its latest-only feed uses transport revisions distinct from presentation revisions and attachment generations.
- Complete snapshots remove omitted surfaces/images. Slow consumers skip intermediate values and repair from the latest complete frame. Delta transport is not implemented.

Surface provenance preserves owner-local identity, retained identity, revision, clipping, ordering, content identity and continuation watermark. It describes authoritative output; it cannot install another compositor.

Metadata availability is explicit in wire version 2. An empty scene does not imply that a producer supplied logical selection or semantics. Per-surface availability describes partial coverage, and unavailable categories are disclosed by the browser. Logical source text must come from the owning producer, never reconstructed from flattened display cells. Native terminal source metadata remains subject to its actual producer contract.

Standalone TUI consumers can prepare the same presentation and use `BrowserFrame::from_prepared`; `packages/web-renderer/examples/snapshot.rs` demonstrates this without native attachment dependencies. Backends report supported features and fail explicitly on unsupported required output rather than silently omit it.

## Compact presentation transport

Live browser delivery negotiates binary presentation v1 through `/view` and
`/presentation?version=1`. The native bridge and WASM use the same `bmux_codec`
positional schema: message-local style/glyph tables, fill/ASCII/glyph row spans,
exact generation/revision delta bases, and complete metadata projection. Only
changed rows and changed metadata/image sections are sent in deltas. Missing or
old transport bases receive the latest compact full snapshot; resize and
replacement generations use snapshots. Browser transport cursors advance only
after successful presentation.

Image placements reference immutable SHA-256 resource IDs. Authenticated
`/resource/<id>` responses carry raw bytes; the host pins bounded latest/previous
manifests. WASM verifies resource digests and descriptions before committing,
retains committed resources on failure, and evicts unreferenced resources after
commit. JavaScript fetches bounded binary buffers and opaque resources; it does
not implement the binary decoder. Unchanged placement payloads reuse decoded
ImageBitmaps.

JSON v2 remains a diagnostic representation at `/snapshot`, not the live wire.
When expanded JSON exceeds its independent byte limit, that endpoint returns
406 while the compact binary presentation remains available. Protocol version
mismatches fail explicitly; rebuild native and WASM assets together.

## Presenter audience output

`present play` and `present host` use the same prepared TUI browser publisher for
an explicit read-only audience projection. See [Presenter browser audience output](presenter-web.md)
for mirror/web-only syntax, private-notes boundaries, configuration precedence,
authorized host navigation, and the separate real-browser acceptance harness.
Shared `[web]`/`BMUX_WEB_*` network settings do not opt presenter output in.
Presenter browser acceptance requires coordinator-built artifacts; documentation
of the contract is not evidence that the runtime suite has passed.

## Acceptance testing

`scripts/smoke-web-browser.sh` runs the real browser acceptance harness with explicit built artifacts and isolated owned runtime state. It requires Python, a Chromium executable, native BMUX, built WASM assets, and the snapshot example's JSON. Missing dependencies are failures, not successful skips. See `--help` for artifact arguments and credential-safe evidence output.

Run repository validation inside the locked shell, build current native/WASM artifacts, then exercise live plugin chrome, terminal updates, images, resize, owner/mirror policy, reconnect, generation repair and backend lifecycle. Fixture decoding alone does not establish live acceptance.

## Future work

Faithful HTML/WebGL backends should implement the same presentation contract with private resource caches. Responsive web-native HTML is a separate presentation policy with its own authoritative geometry, not a backend silently rearranging cells. Interactive browser input, mobile behavior and remote hosting remain outside this release. Unsupported image formats fail explicitly.
