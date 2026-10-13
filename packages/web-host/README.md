# bmux_web_host

Reusable **Unix-native** bounded HTTP hosting for opaque immutable encoded
presentation snapshots. No renderer, terminal, attach, CLI or plugin dependencies.
This crate neither builds browser code nor invents WASM output.

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
provenance, epoch checks, JSON validity, and browser repair/rendering belong to the
caller/browser, not this library. `publish` rejects empty/oversized bytes,
non-increasing revisions and a closed feed. Only the latest snapshot is retained;
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

`start` requires an active Tokio runtime. It always binds `127.0.0.1:0` and creates
a fresh 256-bit OS-random capability per host. It never logs URLs, requests or tokens.
`address()` has no credentials; `bootstrap_url()` returns a sensitive fragment-token
URL. Bootstrap must extract `#token=…`, immediately clear the fragment with history
replacement, keep it only in memory, and use `Authorization: Bearer <token>`.
No cookies, durable credentials, token query parameters or CORS support.

## Browser/HTTP contract

Default exact allowlist (caller can replace `HostConfig.assets`, at most 16 entries):

| URL | File | Type |
|---|---|---|
| `/`, `/index.html` | `index.html` | HTML |
| `/bootstrap.js` | `bootstrap.js` | JavaScript |
| `/bmux_web_renderer.js` | `bmux_web_renderer.js` | JavaScript |
| `/bmux_web_renderer_bg.wasm` | `bmux_web_renderer_bg.wasm` | WASM |

Assets must be real build output in an explicit absolute directory. Every directory
component and asset is opened descriptor-relative without following symlinks.
Only regular nonempty files with flat safe names are accepted, copied into bounded
memory at startup, then served from that immutable copy. No dynamic filesystem route,
directory listing, traversal, percent decoding, wildcard, source maps or auto-discovery.
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
Optional `allowed_hosts` restricts exact authorities including ports; empty means
unrestricted. A supplied Origin must match `http://<request Host>`; mutations
require that Origin. Supplied `Sec-Fetch-Site` must be `same-origin` or `none`.
Cross-origin requests remain rejected; DNS-rebinding host restrictions are opt-in.
Static bootstrap assets are public but host/origin guarded. Snapshots require a
bearer only in explicit token mode; loopback defaults to port 8080 without a token.
`HostConfig.port = 0` selects an ephemeral port; `auth_mode = AuthMode::Token`
restores capability-URL access. Token-free mode exposes output to reachable clients;
external access controls are deployment-owned. Occupied fixed ports fail rather
than selecting another.
`HostConfig.host` accepts IPv4/IPv6, defaulting to `127.0.0.1`. Authentication is
independent: `AuthMode::None` is the default on any bind; `AuthMode::Token` opts in.
Wildcard binds need no public host. Their printed URL uses loopback as a local
example; remote users substitute their reachable IP or DNS alias. Optional
`public_host` changes only the printed URL, never request restrictions or binding.
`allowed_hosts` independently opts into exact Host restrictions (at most 64 valid
authorities). Forwarded host/proto headers do not change the origin policy.
HTTP is unencrypted. With no token, reachable clients can read output and
acquire leased viewport controls; deployment owners provide any external access
controls. Neither mode grants authority beyond this presentation/viewport scope.

All responses disable caches/referrers, deny framing and MIME sniffing, and use CSP:
external same-origin scripts plus `wasm-unsafe-eval`, same-origin connections/styles,
self/blob/data images, no forms/base/frame ancestors. Browser lane must use external
scripts/styles (or set styling through Canvas); inline scripts/styles are not supported.

Strict HTTP/1.1 subset; duplicate headers, folded headers, transfer encoding,
unexpected bodies, upgrades, malformed paths and pipelined GET requests are rejected.
Every response closes its connection. Headers are byte/count bounded with a read
deadline; full-response writes have a deadline (slow consumers are disconnected).
Connection count, poll duration, snapshot bytes, per-file bytes and total asset bytes
have configured budgets and hard ceilings. There are no unbounded queues or detached
connection tasks. Shutdown stops the listener, aborts/drains all connection tasks;
Drop requests shutdown and aborts the owning task, whose JoinSet cancels its children.
Dropping the caller's publisher closes the watch feed and stops the host and its children.

## Optional viewport control

`HostConfig::new` defaults to `ViewportMode::Observer`. `Resizable` enables only
neutral geometry transport; product authorization and effects remain caller-owned.
After publishing successful output, call `host.set_generation(&generation)` with
an opaque 1–128 byte printable ASCII identity. Before that, control is unavailable.
`host.invalidate_view()` revokes the lease and pending requests on source loss.

`host.view_control()` provides a cloneable `ViewControl` exposing only
`set_generation` and `invalidate_view`, suitable for an owned output delivery
closure without exposing the bootstrap capability or lifecycle authority.

All control requests require bearer authentication, exact Host and an explicit exact
Origin (and reject foreign Fetch-Site). `POST /view` with no body claims the first
controller. It returns JSON `{protocol_version:1, generation, viewport_allowed, controller, lease_ms}`;
clients must reject unsupported protocol versions;
additional viewers remain mirrors (`viewport_allowed:false`, `controller:null`).
The generation is authoritative output identity, not a lease ID. The controller is
a fresh independent random secret; never log or persist it. Renew every 10 seconds
by repeating `POST /view` with `X-Bmux-Controller`. The lease lasts 30 seconds;
`DELETE /view` with the same header releases it. A disconnected browser loses its
lease after inactivity; reconnect with the same controller before expiry preserves
ownership and request ordering. After losing a lease, discard the controller and
claim again without the header. Existing viewers cannot resize a live controller.

`POST /viewport` requires `X-Bmux-Controller`, `Content-Type: application/json` and
an exact Content-Length at most 512 bytes. Body is strictly
`{generation:string,request_id:decimal_u64_string,cols:u16,rows:u16}`. Dimensions
must be nonzero, at most 262144 cells. Request IDs increase strictly for the lease,
including across reconnect; no leading zeros. Accepted resize submissions are
limited to 20/sec (429 means retry with the same ID); stale generation/lease/ID
returns 409. No command, terminal-input, or arbitrary application endpoint exists.

Take the one receiver with `host.take_viewport_requests()`. `recv().await` returns
the latest request, skipping intermediate values without queue growth. Check
`requests.is_current(&request)` and the application's authoritative generation
immediately before the effect. A cloned `ViewControl` also exposes
`is_current(&request) -> bool` for revalidation when an asynchronous driver
consumes an enqueued request, before its native sizing effect. For short synchronous effects,
`requests.with_current(&request, || effect())` pins current lease/generation while
running the closure (do not call back into the host). Async effects must revalidate
at their actual application point. Requests contain private owner identity so a
queued old-owner request is invalid after transfer even if generation is unchanged.
Drop/shutdown/producer closure invalidate ownership and wake the receiver closed.

## Validation ownership

Unit tests cover strict parsing, auth/host/origin, smuggling/body/method rejection,
traversal/allowlists, bounded monotonic publication and full-snapshot repair. Isolated
Tokio TCP acceptance tests use explicit temporary assets and ephemeral loopback ports;
they cover live full snapshots/latest replacement, rejection responses, closed-producer
invalidation, budgets, graceful shutdown and Drop cancellation of child connections.
Coordinator owns running these tests, workspace registration, compilation/clippy/full
tests, additional slow-reader checks, and browser/WASM build/integration.
