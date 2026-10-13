# Browser rendering

BMUX shares retained presentation primitives between terminal output and a Rust/WASM browser renderer. Canvas 2D is the initial browser backend; `BrowserBackend` keeps output replaceable without exposing Canvas, DOM, or GPU objects in the shared contract. HTML/WebGL and interactive browser input remain future work.

## Build and attach locally

Use the locked Nix shell throughout:

```sh
bash scripts/dev.sh cargo install wasm-bindgen-cli --version 0.2.126 --locked --root "$PWD/target/web-tools"
bash scripts/dev.sh bash scripts/build-web-renderer.sh
bash scripts/dev.sh cargo run -p bmux_cli --bin bmux -- attach "<SESSION_UUID>" --web-assets "$PWD/target/web-assets" --web-only
```

Replace `<SESSION_UUID>` with an ID from `bmux list-sessions --json`. `--web-only` opens an independent output-only attachment: it does not require terminal input, raw mode, or ANSI output. Its initial viewport is 80×24 until the first browser supplies measured geometry. Stop the owned attachment with Ctrl-C; disconnecting its browser does not terminate the server-owned process.

An independent attachment does not follow another client's focused pane, zoom state, or selected tab. To follow your laptop view, identify that terminal client with `bmux list-clients --json`, then use its **client UUID** (not the session UUID):

```sh
bash scripts/dev.sh cargo run -p bmux_cli --bin bmux -- attach --follow "<LAPTOP_CLIENT_UUID>" --global --web-assets "$PWD/target/web-assets" --web-only
```

`--global` keeps following when that client switches sessions. Choose the client explicitly; BMUX does not guess among attached clients.

Omit `--web-only` to mirror an interactive terminal attachment. In mirror mode the terminal owns viewport dimensions; browser resizing only changes local pixel metrics, not terminal layout. Native provider-only attachment is explicitly unsupported by this entry point.

The default browser URL is **http://127.0.0.1:8080/**: fixed loopback port,
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

Loopback remains the default. Bind your LAN IP with token authentication:

```sh
bmux attach "<SESSION_UUID>" --web-assets "$PWD/target/web-assets" --web-only \
  --web-host 192.168.1.188 --web-auth token
```

Or listen on all IPv4 interfaces with an explicit browser-facing address:

```sh
bmux attach "<SESSION_UUID>" --web-assets "$PWD/target/web-assets" --web-only \
  --web-host 0.0.0.0 --web-public-host 192.168.1.188 --web-auth token
```

Replace the example IP with your serving computer's LAN IP. Open the **printed
capability URL**, including `#token=...`, on the other computer, not `0.0.0.0`.
Allow the TCP port through your firewall only from trusted clients. Never forward
this port through your router to the Internet.

Equivalent configuration:

```toml
[web]
host = "0.0.0.0"
public_host = "192.168.1.188"
port = 8080
auth = "token"
```

`BMUX_WEB_HOST` and `BMUX_WEB_PUBLIC_HOST` are the environment equivalents.
All fields independently honor CLI → environment → config → defaults precedence.
Hosts must be literal IPv4/IPv6 addresses, not DNS names. For IPv6 use
`--web-host ::` and a concrete IPv6 `--web-public-host`; URLs bracket IPv6.
Scoped link-local IPv6 addresses are unsupported. Wildcards require a concrete
public IP in the same family. With concrete binds, public host must match host.
Only the exact advertised Host/Origin is trusted, not arbitrary aliases.

**Security:** every non-loopback bind requires explicit token authentication.
Tokens restrict access but **HTTP is unencrypted**: network observers can read
terminal output and steal tokens. Use only a trusted LAN or encrypted VPN; use an
SSH tunnel for untrusted networks. Native TLS and HTTPS reverse-proxy deployment
are not provided by these options. This capability grants only read-only
presentation and leased viewport control, not terminal input, plugin actions, or
broader BMUX permissions. Token-mode refresh requires reopening the capability URL.

## Viewport ownership and lifecycle

- The first browser claims a separate random controller lease; additional viewers mirror the same composed view.
- Lease renewal occurs periodically; expiry, generation changes, explicit release, or source loss revoke resize authority. An old owner's queued request cannot become a new owner's request.
- Font, container and device-pixel-ratio changes update Canvas metrics. Bounded cell dimensions are sent through an authenticated latest-only channel, not a terminal input endpoint.
- Native sizing uses the existing attachment/PTY policy and authoritative plugin layout; there is no browser-specific competing layout authority.
- Temporary transport loss repairs from a complete snapshot. Source closure shuts down hosting rather than retaining a healthy-looking stale frame. The attachment host remains alive while an independent viewer disconnects; explicit shutdown closes it.

The host defaults to IPv4 loopback port 8080 and supports explicitly authenticated IPv4/IPv6 listeners. It restricts assets to an explicit allowlist, validates configured Host/Origin and capability headers, and bounds bodies, connections, requests and snapshots. Styles are external same-origin assets compatible with the strict CSP. There is no unauthenticated remote deployment mode.

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

## Acceptance testing

`scripts/smoke-web-browser.sh` runs the real browser acceptance harness with explicit built artifacts and isolated owned runtime state. It requires Python, a Chromium executable, native BMUX, built WASM assets, and the snapshot example's JSON. Missing dependencies are failures, not successful skips. See `--help` for artifact arguments and credential-safe evidence output.

Run repository validation inside the locked shell, build current native/WASM artifacts, then exercise live plugin chrome, terminal updates, images, resize, owner/mirror policy, reconnect, generation repair and backend lifecycle. Fixture decoding alone does not establish live acceptance.

## Future work

Faithful HTML/WebGL backends should implement the same presentation contract with private resource caches. Responsive web-native HTML is a separate presentation policy with its own authoritative geometry, not a backend silently rearranging cells. Interactive browser input, mobile behavior and remote hosting remain outside this release. Unsupported image formats fail explicitly.
