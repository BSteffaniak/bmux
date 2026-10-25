# Live stream protocol v1

The Rust `bmux-live-relay` executable is a single-channel, persistent WebSocket
relay. It carries the existing **BMWP version 5** presentation protocol from
`packages/web-renderer/src/wire.rs`; it does not define another scene model.
This is a coordinated binary upgrade: v5 adds the positional
`SemanticRegion.accessibility` option, including its explicit absent value. JSON
remains v3. Native/WASM consumers, relay negotiation (`bmwp-5`), and immutable
viewer assets must upgrade together; binary v1–v4 frames are rejected before
metadata decoding. Authentication, fencing, and service interfaces are
unchanged.
All connections use TLS at the Fly edge. Internal listener: `0.0.0.0:8080`.
No query parameters, channel IDs, cookies, or subprotocol negotiation are used.

## Publisher authority and HTTP

Every `/v1/publisher/*` request requires:

```http
Authorization: Bearer <publisher key>
```

The server compares SHA-256(key) against `PUBLISHER_KEY_SHA256`, a lowercase
64-character hex digest. It rejects publisher requests with an `Origin` header.
Keys never appear in URLs, response bodies, metrics, or durable authority state.

`POST /v1/publisher/start`, `Content-Type: application/json`, body:

```json
{"publisher_id":"c735c094-a3c8-486c-b240-10e799cae79f"}
```

The UUID is publisher identity, not a credential. An unexpired owner makes start
return HTTP 409 (even for the same publisher). On success HTTP 200 body:

```json
{"type":"state","epoch":"06172cac-1737-491d-82cd-a1f3c343baec","fence":"2","state":"starting","lease_ms":15000,"lease_remaining_ms":15000}
```

Every control request and publisher WebSocket upgrade additionally requires:

```http
X-Bmux-Epoch: 06172cac-1737-491d-82cd-a1f3c343baec
X-Bmux-Fence: 2
```

Fence and revision values in JSON are decimal **strings**, not JSON numbers.
Control routes are POST, with no body:

- `/v1/publisher/heartbeat`: renew the 15-second lease; call at least every 5s.
- `/v1/publisher/pause`: advance fence, clear retained frame, close old publisher
  socket, publish paused state. Response contains the new fence.
- `/v1/publisher/resume`: allowed from paused only; advance fence, clear baseline,
  enter starting. Reconnect publisher WS with the returned fence and send full.
- `/v1/publisher/stop`: advance fence, clear/revoke epoch and publisher ownership,
  enter offline. Previous credentials cannot revive that epoch.

Successful controls return the same state object schema as start. Offline has
`epoch:null`. Invalid credentials: 401; browser publisher Origin: 403;
stale/expired epoch or fence, active start, paused WS: 409; budget exhausted:
429;
persistence unavailable: 503. Authority is checked again at every admitted
frame.

## Publisher WebSocket

`GET /v1/publisher/ws` uses all three headers above. Exactly one publisher
socket
is admitted. Each binary WS message is exactly one unchanged BMWP v5 full or
delta message. Full snapshots embed all needed immutable image resources; deltas
embed only new resources and inherit unchanged resources from their exact base.
The relay validates against resources from that retained base and forwards
original
patch bytes unchanged. Immutable bytes are not retransmitted for tiny cell
updates.
There is no
outer binary envelope, compression bundle, HTTP polling, or resource URL.
WS fragmentation is handled by the transport; limits apply to the reassembled
message. Client sends one message, waits for its ACK, then sends another:

```json
{"type":"ack","generation":"source-generation","revision":"42"}
```

ACK means the relay validated and retained the frame; it does not mean viewers
have drawn it. Full snapshot is mandatory after each publisher WS connection,
restart, resume, or repair. A full may change generation. Within one generation
revisions must strictly increase. Deltas must name the exact retained
generation,
revision and dimensions and pass the canonical renderer decoder, including
resource digests, metadata, styles, rows and placements. On validation failure:

```json
{"type":"repair"}
```

The client must then send a fresh full snapshot. It must not resend an already
ACKed revision. Oversized WS messages close the connection. Binary frames do not
renew the lease; HTTP heartbeat is intentionally separate from rendering.

## Viewer WebSocket

`GET /v1/viewer/ws` needs no credential; it **requires** an exact allowed
`Origin`
header, by default `https://bmux.dev` or `https://www.bmux.dev`. No URL
parameters
or client identity headers are needed. Unlisted or absent Origin is HTTP 403.
This is public output, not an input/control endpoint.

On connection and authority state changes, the relay sends a text state object:

```json
{"type":"state","epoch":null,"fence":"3","state":"offline","lease_ms":15000,"lease_remaining_ms":0}
```

States: `offline`, `starting`, `live`, `paused`. Every state message includes
`lease_remaining_ms`, clamped to 0..15000, calculated at send time. Successful
publisher heartbeats emit state renewals without binary render traffic. The
browser must hide/clear output when that duration elapses without renewal,
even if WS remains connected. Pong and binary frames never extend this display
lease. A zero duration hides output immediately. Use a monotonic browser timer.
This bounds stale visibility independently of a stalled socket.
On epoch/fence change the viewer
must clear its old baseline. While live the first binary message is a BMWP v5
full snapshot. After successfully decoding/applying each binary message send:

```json
{"type":"ack","generation":"source-generation","revision":"42"}
```

Only one binary message is in flight per viewer. ACK identity must exactly match
that message; unsolicited/mismatched ACK closes the socket. To repair decoding:

```json
{"type":"repair"}
```

Repair discards viewer cursor and requests the newest full snapshot. Fast
viewers
receive original publisher deltas only when their ACKed cursor equals the delta
base. Slow viewers retain no queue: after ACK the latest frame is delivered as
full if its delta cannot continue the viewer cursor. No dropped delta is ever
presented as a complete continuation. Full encoding is shared/cached per
retained
revision and is materialized only when a viewer needs it. A new-generation full
is self-describing through BMWP. Embedded resources avoid a competing cache API.

Pause/stop/expiry invalidate pending old-epoch output. Clients must process
state
before subsequent binary frames and clear baseline on fence changes. The relay
closes slow sockets rather than allowing writes or ACKs to wait indefinitely.

## Bounds and idle behavior

Default bounds: 64 viewers, one publisher, 16 simultaneous control operations,
8 MiB reassembled publisher messages, 1 KiB HTTP control bodies/viewer messages,
10s send/ACK deadline. No per-viewer frame queue; each viewer holds at most one
pending identity and bounded shared frame references. Publisher processing is
serial with ACK backpressure. Configurable limits and ingress rate budgets are
listed with deployment configuration below. HTTP has a global fixed-window
32 requests/s limit (health exempt); at most 16 concurrent requests and a 10s
handler/body deadline. Publisher ingress uses process-global one-second fixed
windows, so reconnecting does not reset the budget; exceeding rate closes WS.
Viewer controls are limited to 120 messages / 64 KiB per second per connection.

At idle there are no render messages, snapshots, uploads, or polling. WS ping /
pong every 5s is independent keepalive; missing pong for 15s disconnects. Lease
expiry is evaluated at 1s intervals and on authorization checks.

## Durability and deployment

`/data/relay/authority.json` stores versioned minimal authority metadata: epoch,
publisher ID, fence, phase, absolute lease expiry. No frame, resource, revision,
key, or viewer state is written. Transitions write a temporary file, fsync it,
rename atomically, and fsync the parent. Malformed/unsupported metadata fails
startup closed. Exactly one relay process may own a volume/channel; horizontal
multi-writer replication is not supported. On restart fence advances durably;
unexpired ownership remains reserved, but a fresh full snapshot is mandatory.
Publisher recovery uses authenticated `GET /v1/publisher/status` with the
`X-Bmux-Epoch` header (and Authorization, without X-Bmux-Fence). It returns the
state object/current fence only for that still-active epoch; expired/revoked
epochs return 409. It cannot mutate or renew authority. Thus an interrupted
control response or relay restart can be recovered without starting another
owner.

Required env: `PUBLISHER_KEY_SHA256`, `METRICS_KEY_SHA256` (distinct hashes).
Optional: `AUTHORITY_PATH` (default `/data/relay/authority.json`),
`ALLOWED_ORIGINS`
(comma-separated exact HTTPS bmux.dev/subdomain origins; no wildcard, path,
port,
or arbitrary third-party origin). Resource budgets:

| Variable                             |  Default |    Maximum |
| ------------------------------------ | -------: | ---------: |
| `MAX_VIEWERS`                        |       64 |       1024 |
| `MAX_MESSAGE_BYTES`                  |  8388608 |    8388608 |
| `MAX_FRAMES_PER_SECOND`              |       60 |        240 |
| `MAX_BYTES_PER_SECOND`               | 33554432 |  134217728 |
| `MAX_VIEWER_EGRESS_BYTES_PER_SECOND` | 16777216 |  134217728 |
| `MAX_EGRESS_BYTES_PER_SECOND`        | 67108864 | 1073741824 |

Values must be positive decimal integers; invalid configuration fails startup.
Every binary viewer payload (initial/reconnect full, normal patch, slow-viewer
full and explicit repair full) is admitted against per-connection and shared
process-wide byte budgets **before** writing. The aggregate admission is
serialized
and is not reset by viewer reconnect, publisher reconnect, or authority changes.
Budgets use monotonic one-second fixed windows; boundary bursts may span two
windows. Per-viewer windows reset on viewer reconnect, but aggregate limits
still
apply. Defaults permit a full 8 MiB snapshot while bounding repeated repair and
fanout. An exhausted budget immediately disconnects the viewer: no byte queue,
no delayed baseline and no skipped patch masquerading as complete continuation.
Writes which fail remain charged conservatively. A limit below a snapshot size
intentionally refuses that snapshot by disconnecting; do not configure that for
expected frames. Viewer repair controls also share the 120 messages / 64 KiB
per-second connection ingress limit.

These budgets measure BMWP binary payload bytes, not TLS/WS headers or bounded
state/ping/pong traffic. They are process-lifetime rate controls, **not**
durable
usage accounting or a monthly billing cap. Private Prometheus counters
`bmux_relay_viewer_egress_throttled_total` and
`bmux_relay_aggregate_egress_throttled_total` count rejected send admissions by
the first exhausted budget; `bmux_relay_delivery_bytes_total` counts successful
binary payload sends.
Encoded repair snapshots exceeding the hard 8 MiB bound disconnect the viewer
rather than silently skipping a baseline. Metrics include accepted frames,
repairs, uploaded/delivered wire bytes, and current viewer count.

`GET /health` returns HTTP 200 JSON `{"status":"ok","renderer":"bmwp-5"}`.
The renderer identity derives from the shared `BINARY_WIRE_VERSION` constant.
`GET /metrics` requires
`Authorization: Bearer <metrics key>` against `METRICS_KEY_SHA256`; it returns
Prometheus text without identities, keys or rendered content. Do not expose its
credential to viewers. Health does not return authority or credentials.

Legacy JS Worker/polling is not a compatible fallback. Cutover must switch the
publisher and browser together, stop the legacy service, and use a single Fly
machine with the persistent data volume. Never deploy both as channel authority.

## Image build contract

The coordinator builds `bmux-live-relay` release under the flake.lock-selected
Nix shell. Then run `bash packages/live-relay/package-rootfs.sh` with that
explicit
release executable path and `artifacts/live-relay-rootfs` as output. The script
requires a fresh output path, verifies dynamic Nix dependencies are resolved,
copies their complete store closure and records the executable SHA-256.
`docker build -f packages/live-relay/Dockerfile .` copies this rootfs into
scratch.
It does not fetch a compiler or packages, and preserves the actual Nix ELF
loader
path. No Debian/musl compatibility is assumed. Image contains
`/bin/bmux-live-relay`
and its `/nix/store` closure; writable data volume at `/data`, port 8080.
The image starts as UID/GID 0 solely for fixed-path volume initialization.
Before
constructing Tokio, opening sockets, or reading credential environment, startup
opens `/data` with `O_DIRECTORY|O_NOFOLLOW`, requires root ownership and no
group/
world write permission, and permits only `relay` plus a root-owned non-writable
`lost+found`. It creates `/data/relay` with mode 0700, chowns **only that newly
created descriptor** to UID/GID 65532, and fsyncs it and its parent. It never
recursively chowns the mount or any configured path. Existing relay directories
must already have exactly that owner/group/mode. Existing entries may only be
`authority.json` and `authority.tmp`, regular single-link files, UID/GID 65532,
mode 0600 and at most 4096 bytes. Symlinks, foreign files/owners, unsafe modes,
and old `/data/authority.json` are refused rather than migrated/adopted.

Root startup accepts only the fixed `AUTHORITY_PATH=/data/relay/authority.json`
(or an absent variable). It clears supplementary groups, sets real/effective/
saved GID then UID to 65532, sets Linux `PR_SET_NO_NEW_PRIVS`, verifies the
drop,
and only then starts the runtime and reads credential env. Any failure exits.
Nonroot test execution does not initialize/chown the mount and may supply its
own
`AUTHORITY_PATH`; ordinary filesystem permissions apply. A mounted volume must
retain `/data/relay` across restarts. The old flat path is **not an alias**.

`python3 packages/live-relay/test-container-startup.py <explicit-built-binary>`
builds the actual scratch image with that binary's Nix closure, uses a fresh
isolated Docker volume and synthetic keys, verifies HTTP health, UID/GID 65532,
`NoNewPrivs=1`, authority file mode/owner, restart and foreign-symlink refusal,
and cleans its own image/containers/volume. No shell is installed in the image.
The image build must fail if the prebuilt artifact/closure is absent.
