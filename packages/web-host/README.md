# bmux_web_host

Reusable **Unix and Windows native** bounded HTTP hosting for opaque immutable
encoded
presentation snapshots. The optional `presentation` feature provides native
renderer integration.
Native compilation records source provenance only: it never compiles WASM. Web
callers
explicitly opt into first-use asset setup; terminal-only operation never needs
setup.

## Platform boundaries

HTTP, authentication, budgets, publication, and shutdown share one
implementation.
Unix assets retain component-wise `openat`/`O_NOFOLLOW`; Windows assets use
retained
local-drive directory handles and relative NT opens, rejecting reparse points
(including junctions), UNC/device paths, and non-regular files. Paths must be
absolute. Assets are copied into bounded immutable memory before binding.

Windows resolver caches use current-user owner/private DACLs, a kernel file
lock,
flushed files, and a write-through non-replacing publication move. The default
cache is `%LOCALAPPDATA%/bmux/cache`; explicit isolated-runtime cache selection
still takes precedence. Existing public or redirected caches fail closed.
Verified release bundles, offline cached bundles, and explicit `--web-assets`
directories are supported on Windows. Source-first-use compilation still
requires
the Unix Nix/Bash driver: native Windows reports an explicit unsupported error
rather than invoking an ambient unpinned compiler. Native builds do not run
setup.

## Semantic document delivery

`DocumentPublication { generation, revision, capability, content: Arc<[u8]> }`
contains opaque product bytes; the host never parses them or owns navigation.
`SnapshotPublisher::publish_document(transport_revision, document, resources)`
atomically replaces content and its complete SHA-256 resource manifest.
Transport
revisions strictly increase. Document revisions are positive and strictly
increase
within a generation; changing capability requires a new generation. Failed
admission
preserves the previous publication. Fixed-frame APIs remain unchanged.

`GET /document?version=1[&after=<transport revision>]` is authenticated by the
same
policy as snapshots. It returns `application/vnd.bmux.document-v1+json` and
`X-Snapshot-Revision`. Full-only JSON schema:

```json
{"version":1,"publication_revision":"42","generation":"source-v1","revision":"7","capability":"application.content.v1","content":[123,125],"resources":[{"id":"<lowercase SHA-256>","mime_type":"image/png","length":123}]}
```

Both revisions are decimal strings, never JavaScript numbers. Content is a
nonempty
byte array; maximum source bytes are one quarter of the smaller of the
configured
feed limit and 16 MiB, bounding JSON byte-array expansion. The entire serialized
envelope must also fit that smaller limit. Generation/capability are 1–128
ASCII
alphanumeric characters or `._:-`. Resources retain the fixed transport's 8 MiB
per resource, 1024 entries, and 32 MiB latest-plus-previous admission bounds.
`/resource/<id>` remains the exact resource endpoint. A caught-up poll returns
204;
wrong representation returns 406; closed sources return 503. Repair fetches a
full
document without `after`; there is no delta or independent publication queue.

The generic index defaults to fixed frames. Applications may set
`HostConfig::browser_delivery` to `BrowserDelivery::Document { adapter }` so `/`
opens their document viewer directly. The host validates the adapter against its
JavaScript asset allowlist and embeds these defaults in the bootstrap at
startup,
without redirects or extra configuration requests. Presenter hosts select their
own adapter this way; the host does not interpret application semantics.
Explicit `delivery` and `adapter` query parameters override host defaults, so
`/?delivery=document&adapter=/application-adapter.js` remains supported
(followed
by the existing token fragment when applicable). Unknown delivery modes are
rejected. Document bootstrap capability-checks a caller-provided
same-origin module, locally reflows independent browser viewports, and never
acquires
or mutates a shared viewport lease. The explicit bundle allowlist includes the
presenter adapter and existing presenter WASM crate as packaging assets only; no
presentation semantics live in the host.

## First-use assets

`resolve_assets(override_directory).await` returns a verified cached directory
or
performs setup once. `Some(path)` bypasses setup; `start` still validates and
loads
the configured asset allowlist before binding. Cached use is offline and silent;
progress appears on stderr only during setup.

Product callers use
`resolve_assets_with_cache(override_directory, cache_root).await`, applying
`BMUX_CACHE_DIR` precedence themselves, otherwise passing the authoritative
selected `state_dir.join("cache")`. The resolver adds `web-assets/` beneath that
root, and never mutates environment variables. Convenience resolution uses
`BMUX_CACHE_DIR`, then `XDG_CACHE_HOME/bmux`, then `HOME/.cache/bmux`;
non-default
runtime/slot/target contexts require the explicit cache API or `BMUX_CACHE_DIR`.
Roots must be absolute and real directories, not symlinks.

Source builds capture an absolute root and SHA-256 in `build.rs`. Before
executing
`nix develop --command`, runtime recomputes and compares the input identity.
Missing
or changed sources fail explicitly; an existing root is not sufficient
authority.
The pinned Nix environment owns compiler and target-directory selection.
Matching
`wasm-bindgen-cli` is installed only into a staged private cache root using its
exact version and locked dependencies, never into a system or default Cargo bin
location. Its cached binary hash and version are checked before reuse. Builds
use
`cargo build --locked -p bmux_web_renderer --features wasm --target
wasm32-unknown-unknown`.

Release builds embed an exact CI-supplied `BMUX_WEB_RELEASE_TAG`; absent tags
mean
source provenance mode, and `latest` is rejected. Releases never fall back to
source or another release. HTTPS downloads come from that exact GitHub release:
`bmux-web-assets-{renderer_sha256}.json` and
`bmux-web-assets-{renderer_sha256}-{name}`. The strict schema-1 manifest
contains
`version` (package version), `renderer_sha256`, `wasm_bindgen_version`, and
exactly
five `{name,size,sha256}` entries: `index.html`, `bootstrap.js`, `style.css`,
`bmux_web_renderer.js`, `bmux_web_renderer_bg.wasm`. Limits are 16 KiB manifest,
8 MiB per file, 24 MiB aggregate. SHA-256 and WASM magic/version are verified.

The shared `src/provenance.rs` and `renderer-identity` example define release
identity:
recursively selected local normal/build dependencies (including
target-specific),
package manifests, `src/**`, `browser/**`, `build.rs`, plus workspace
manifest/lock,
`flake.nix`, `flake.lock`, and `.cargo/config.toml`. Paths are
sorted
UTF-8 relative paths normalized with `/` separators (including on Windows). Hash
framing starts with `bmux-web-renderer-inputs-v1\0`, then
for each input: LE-u64 path-byte length, path bytes, LE-u64 content-byte length,
content bytes. Workspace manifest/lock changes conservatively invalidate
identity,
even for unrelated dependencies. The example accepts the source root and prints
only the shared SHA-256.

Bundles are keyed by package version, release tag (or source), and renderer
hash.
Interprocess locks serialize setup. Files, staging directories, rename parents,
and newly created cache parents are synced before success. Existing corrupt or
unsupported cache/tool state produces errors without deletion or automatic
repair;
explicitly move the affected cache aside before retrying. Incomplete temporary
setup is never accepted as a published bundle.

## Integration

```rust,no_run
use std::{path::PathBuf, sync::Arc};
use bmux_web_host::{HostConfig, snapshot_feed, start};

# async fn example(encoded_json: Arc<[u8]>) -> std::io::Result<()> {
let config = HostConfig::new(PathBuf::from("/absolute/browser-build-output"));
let (publisher, feed) = snapshot_feed(config.limits.max_snapshot_bytes)?;
let host = start(config, feed).await?;
publisher.publish(1, encoded_json)?;
// Explicitly deliver host.bootstrap_url() to the user; NEVER log it.
// Keep publisher and host alive while the caller's observation is active.
host.shutdown().await;
# Ok(())
# }
```

A caller bridges its committed prepared presentation to encoded full snapshots.
The transport revision must strictly advance for this host lifetime, **even when
an encoded frame's epoch/revision resets**. Encoded presentation semantics,
provenance, epoch checks, JSON validity, and browser repair/rendering belong to
the
caller/browser, not this library. `publish` rejects empty/oversized bytes,
non-increasing revisions and a closed feed. Only the latest snapshot is
retained;
slow readers may miss intermediates but always receive a complete full snapshot.

`publish_representations(revision, json: Option<Arc<[u8]>>, binary, resources)`
atomically publishes all representations. `BinaryPresentation` contains a full
`snapshot` and optional `delta: Option<(base_transport_revision, bytes)>`; the
base must be the immediately preceding binary publication. JSON `None` leaves
binary available and makes diagnostic `/snapshot` return `406`. Existing
`publish(revision, json)` remains unchanged.

* `GET /presentation?version=1&after=<u64>&base=<u64>` requires the bearer and
  existing Host/Origin protection. Version is mandatory; unsupported versions,
  unknown/duplicate fields and malformed cursors return `400`. `after` and `base`
  are optional. A matching exact base selects the latest delta; all other bases
  select the full snapshot. Polling waits only when `after == base == latest`;
  a mismatched committed base repairs immediately. Responses use
  `application/vnd.bmux.presentation-v1` and `X-Snapshot-Revision`.
* `/view` advertises `presentation_versions: [1]` in addition to its existing
  control protocol fields.
* `GET /resource/<id>` requires the same auth/Origin checks. IDs are lowercase
  SHA-256 hex of opaque bytes, verified on publication. `Resource` contains
  `bytes: Arc<[u8]>` and `mime_type: String`; HTTP length derives from bytes.
  Reusing a retained ID requires identical bytes and MIME. Complete manifests
  are retained for only latest/previous publications; expired IDs return `404`.
  There is no resource queue, discovery, or domain interpretation. Limits are
  8 MiB/resource, 1,024 entries/manifest, and 32 MiB for both retained manifests
  (shared entries count conservatively twice). Every rejection is atomic.

`start` requires an active Tokio runtime. It always binds `127.0.0.1:0` and
creates
a fresh 256-bit OS-random capability per host. It never logs URLs, requests or
tokens.
`address()` has no credentials; `bootstrap_url()` returns a sensitive
fragment-token
URL. Bootstrap must extract `#token=…`, immediately clear the fragment with
history
replacement, keep it only in memory, and use `Authorization: Bearer <token>`.
No cookies, durable credentials, token query parameters or CORS support.

## Browser/HTTP contract

Default exact allowlist (caller can replace `HostConfig.assets`, at most 16
entries):

| URL                          | File                        | Type       |
| ---------------------------- | --------------------------- | ---------- |
| `/`, `/index.html`           | `index.html`                | HTML       |
| `/bootstrap.js`              | `bootstrap.js`              | JavaScript |
| `/bmux_web_renderer.js`      | `bmux_web_renderer.js`      | JavaScript |
| `/bmux_web_renderer_bg.wasm` | `bmux_web_renderer_bg.wasm` | WASM       |

Assets must be real build output in an explicit absolute directory. Every
directory
component and asset is opened descriptor-relative without following symlinks.
Only regular nonempty files with flat safe names are accepted, copied into
bounded
memory at startup, then served from that immutable copy. No dynamic filesystem
route,
directory listing, traversal, percent decoding, wildcard, source maps or
auto-discovery.
Configured build directory and explicit allowlist are trusted caller input.

* `GET /snapshot`: authenticated latest full encoded bytes.
* `GET /snapshot?after=<u64>`: authenticated longpoll while current revision equals
  `after`. Different revisions (including future values) return the latest full
  snapshot immediately for repair.
* `200`: `application/json`, full snapshot, `X-Snapshot-Revision: <u64>`.
* `204`: bounded poll timeout; retry with the same revision.
* `503`: no snapshot yet or producer closed (retained snapshots are not served
  as healthy live data after closure).
* `401`: missing/invalid bearer capability. `403`: wrong Host/Origin/fetch-site.
* `405`: all non-GET methods, including terminal input, upgrades and mutations.
  Unknown GET paths are `404` and cannot invoke any operation.

By default any valid HTTP Host authority is accepted, including DNS aliases.
Optional `allowed_hosts` restricts exact authorities including ports; empty
means
unrestricted. A supplied Origin must match `http://<request Host>`; mutations
require that Origin. Supplied `Sec-Fetch-Site` must be `same-origin` or `none`.
Cross-origin requests remain rejected; DNS-rebinding host restrictions are
opt-in.
Static bootstrap assets are public but host/origin guarded. Snapshots require a
bearer only in explicit token mode; loopback defaults to port 8080 without a
token.
`HostConfig.port = 0` selects an ephemeral port; `auth_mode = AuthMode::Token`
restores capability-URL access. Token-free mode exposes output to reachable
clients;
external access controls are deployment-owned. Occupied fixed ports fail rather
than selecting another.
`HostConfig.host` accepts IPv4/IPv6, defaulting to `127.0.0.1`. Authentication
is
independent: `AuthMode::None` is the default on any bind; `AuthMode::Token` opts
in.
Wildcard binds need no public host. Their printed URL uses loopback as a local
example; remote users substitute their reachable IP or DNS alias. Optional
`public_host` changes only the printed URL, never request restrictions or
binding.
`allowed_hosts` independently opts into exact Host restrictions (at most 64
valid
authorities). Forwarded host/proto headers do not change the origin policy.
HTTP is unencrypted. With no token, reachable clients can read output and
acquire leased viewport controls; deployment owners provide any external access
controls. Neither mode grants authority beyond this presentation/viewport scope.

All responses disable caches/referrers, deny framing and MIME sniffing, and use
CSP:
external same-origin scripts plus `wasm-unsafe-eval`, same-origin
connections/styles,
self/blob/data images, no forms/base/frame ancestors. Browser lane must use
external
scripts/styles (or set styling through Canvas); inline scripts/styles are not
supported.

Strict HTTP/1.1 subset; duplicate headers, folded headers, transfer encoding,
unexpected bodies, upgrades, malformed paths and pipelined GET requests are
rejected.
Every response closes its connection. Headers are byte/count bounded with a read
deadline; full-response writes have a deadline (slow consumers are
disconnected).
Connection count, poll duration, snapshot bytes, per-file bytes and total asset
bytes
have configured budgets and hard ceilings. There are no unbounded queues or
detached
connection tasks. Shutdown stops the listener, aborts/drains all connection
tasks;
Drop requests shutdown and aborts the owning task, whose JoinSet cancels its
children.
Dropping the caller's publisher closes the watch feed and stops the host and its
children.

## Optional viewport control

`HostConfig::new` defaults to `ViewportMode::Observer`. `Resizable` enables only
neutral geometry transport; product authorization and effects remain
caller-owned.
After publishing successful output, call `host.set_generation(&generation)` with
an opaque 1–128 byte printable ASCII identity. Before that, control is
unavailable.
`host.invalidate_view()` revokes the lease and pending requests on source loss.

`host.view_control()` provides a cloneable `ViewControl` exposing only
`set_generation` and `invalidate_view`, suitable for an owned output delivery
closure without exposing the bootstrap capability or lifecycle authority.

All control requests require bearer authentication, exact Host and an explicit
exact
Origin (and reject foreign Fetch-Site). `POST /view` with no body claims the
first
controller. It returns JSON `{protocol_version:1, generation, viewport_allowed,
controller, lease_ms}`;
clients must reject unsupported protocol versions;
additional viewers remain mirrors (`viewport_allowed:false`, `controller:null`).
The generation is authoritative output identity, not a lease ID. The controller
is
a fresh independent random secret; never log or persist it. Renew every 10
seconds
by repeating `POST /view` with `X-Bmux-Controller`. The lease lasts 30 seconds;
`DELETE /view` with the same header releases it. A disconnected browser loses
its
lease after inactivity; reconnect with the same controller before expiry
preserves
ownership and request ordering. After losing a lease, discard the controller and
claim again without the header. Existing viewers cannot resize a live
controller.

`POST /viewport` requires `X-Bmux-Controller`, `Content-Type: application/json`
and
an exact Content-Length at most 512 bytes. Body is strictly
`{generation:string,request_id:decimal_u64_string,cols:u16,rows:u16}`.
Dimensions
must be nonzero, at most 262144 cells. Request IDs increase strictly for the
lease,
including across reconnect; no leading zeros. Accepted resize submissions are
limited to 20/sec (429 means retry with the same ID); stale generation/lease/ID
returns 409. No command, terminal-input, or arbitrary application endpoint
exists.

Take the one receiver with `host.take_viewport_requests()`. `recv().await`
returns
the latest request, skipping intermediate values without queue growth. Check
`requests.is_current(&request)` and the application's authoritative generation
immediately before the effect. A cloned `ViewControl` also exposes
`is_current(&request) -> bool` for revalidation when an asynchronous driver
consumes an enqueued request, before its native sizing effect. For short
synchronous effects,
`requests.with_current(&request, || effect())` pins current lease/generation
while
running the closure (do not call back into the host). Async effects must
revalidate
at their actual application point. Requests contain private owner identity so a
queued old-owner request is invalid after transfer even if generation is
unchanged.
Drop/shutdown/producer closure invalidate ownership and wake the receiver
closed.

## Validation ownership

Unit tests cover strict parsing, auth/host/origin, smuggling/body/method
rejection,
traversal/allowlists, bounded monotonic publication and full-snapshot repair.
Isolated
Tokio TCP acceptance tests use explicit temporary assets and ephemeral loopback
ports;
they cover live full snapshots/latest replacement, rejection responses,
closed-producer
invalidation, budgets, graceful shutdown and Drop cancellation of child
connections.
Coordinator owns running these tests, workspace registration,
compilation/clippy/full
tests, additional slow-reader checks, and browser/WASM build/integration.
