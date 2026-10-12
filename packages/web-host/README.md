# bmux_web_host

Reusable **Unix-native**, read-only loopback HTTP hosting for opaque immutable encoded
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

Only exact `Host: 127.0.0.1:<bound-port>` is accepted (not localhost or arbitrary
loopback aliases). A supplied Origin must exactly equal that host's HTTP origin;
supplied `Sec-Fetch-Site` must be `same-origin` or `none`. This blocks DNS rebinding,
foreign-origin requests and sibling-origin fetches even if a capability leaks.
Static bootstrap assets are public but host/origin guarded; snapshots require the token.
All responses disable caches/referrers, deny framing and MIME sniffing, and use CSP:
external same-origin scripts plus `wasm-unsafe-eval`, same-origin connections/styles,
self/blob/data images, no forms/base/frame ancestors. Browser lane must use external
scripts/styles (or set styling through Canvas); inline scripts/styles are not supported.

Strict HTTP/1.1 GET-only subset; duplicate headers, folded headers, transfer encoding,
nonzero bodies, upgrades, malformed paths and pipelined requests are rejected.
Every response closes its connection. Headers are byte/count bounded with a read
deadline; full-response writes have a deadline (slow consumers are disconnected).
Connection count, poll duration, snapshot bytes, per-file bytes and total asset bytes
have configured budgets and hard ceilings. There are no unbounded queues or detached
connection tasks. Shutdown stops the listener, aborts/drains all connection tasks;
Drop requests shutdown and aborts the owning task, whose JoinSet cancels its children.
Dropping the caller's publisher closes the watch feed; caller should also stop the host.

## Validation ownership

Unit tests cover strict parsing, auth/host/origin, smuggling/body/method rejection,
traversal/allowlists, bounded monotonic publication and full-snapshot repair. Isolated
Tokio TCP acceptance tests use explicit temporary assets and ephemeral loopback ports;
they cover live full snapshots/latest replacement, rejection responses, closed-producer
invalidation, budgets, graceful shutdown and Drop cancellation of child connections.
Coordinator owns running these tests, workspace registration, compilation/clippy/full
tests, additional slow-reader checks, and browser/WASM build/integration.
