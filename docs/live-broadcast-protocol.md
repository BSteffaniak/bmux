# Live broadcasting protocol v1

BMUX live broadcasting reuses self-contained `BMWP\x02` full presentations. This is an output-only transport, not terminal input, navigation, or viewport control. Local operation never depends on the relay.

## HTTP contract

All routes are under `/api/live/{channel}`. Channels match `[a-z0-9][a-z0-9-]{0,47}`. No CORS. Production requires HTTPS; loopback HTTP is allowed for local development. Errors are content-free JSON. No bearer, request bodies, terminal content, or publisher IDs may appear in logs.

Publisher routes require `Authorization: Bearer <channel-publisher-key>`. Keys are provisioned separately from deployment credentials. Relay configuration initially maps one configured channel (`main` by default) to a SHA-256 publisher-key digest (`PUBLISHER_KEY_SHA256`), never plaintext. Requests other than start additionally require `X-Bmux-Epoch: <epoch>`.

- `POST /publisher/start`: JSON `{protocol:1,renderer:"bmwp-2",publisher_id:"<UUID>"}`. Returns 200 JSON `{protocol:1,renderer:"bmwp-2",epoch:"<UUID>",lease_ms:15000}`. Creates `starting`; rejects another unexpired owner with 409. Exact retries by the same private publisher ID return the same epoch without undoing pause. Unsupported protocol/renderer is rejected. Expired ownership cannot be revived by an old epoch. No implicit takeover.
- `PUT /publisher/frame`: self-contained full BMWP v2 bytes, `Content-Type: application/octet-stream`, `X-Bmux-Revision: <positive decimal u64>`. Maximum 8 MiB (8,388,608 bytes). Monotonically advancing within an epoch; exact revision+bytes retry is idempotent; conflicting/stale revisions rejected. Valid header/full binary envelope admission is required before publication; complete Rust validation remains the producer/viewer authority. Commit atomically replaces the frame, moves starting/live to live, and renews lease. Reject while paused. Reply 204.
- `POST /publisher/heartbeat`: renew live/starting/paused lease; reply 204. Heartbeat never changes paused/starting to live.
- `POST /publisher/pause`: clear public frame and mark paused; reply 204. In-flight pre-pause frames must not repaint after this transition. Native publisher gates content before uploading and serializes mutations against pause.
- `POST /publisher/resume`: only paused -> starting, frame absent; fresh full frame required for live. Reply 204.
- `POST /publisher/stop`: revoke epoch, clear public frame, offline; reply 204. Idempotent retry for the revoked epoch is allowed without affecting any replacement epoch.

Relay must fence stale requests, enforce actual lease expiry on every access, and bound pending uploads, retained bytes, clients, and body reads. Persist enough lease/epoch/control state to prevent restart undoing pause or resurrecting stopped publishers. Frames may be volatile: after relay restart require a fresh full frame and report starting, not live. Reject deleted/unknown channels. Native requests have deadlines and redirects disabled. Do not log credential URLs or raw transport errors.

Viewer routes require no credential:

- `GET /status`: JSON `{protocol:1,renderer:"bmwp-2",state:"offline|starting|live|paused",epoch:null|"<UUID>",revision:"0"|"<u64>",lease_expires_at:null|<Unix milliseconds>}`. Expired becomes offline, not a frozen live frame. `revision` is the relay delivery cursor, independent of the source revision embedded in BMWP. Cache-Control no-store.
- `GET /presentation?epoch=<epoch>&after=<revision>`: 200 application/octet-stream full current frame with `X-Bmux-Epoch` and `X-Snapshot-Revision`; 204 if same cursor; 409 if epoch mismatches; 503 when not live/no frame. `after` is optional. May return immediately; viewer polls at bounded interval (initially 1 second) with independent status polling to promptly hide paused/offline content. No resource routes are needed for self-contained presentations.
- `GET /health`: content-free versioned relay health; no secrets.

## Viewer and local control

Viewer uses the existing Rust/WASM Canvas renderer with inline resources and validates all bytes. Public viewport is fixed by producer; viewer scales/letterboxes. No `/view`, `/viewport`, keyboard, navigation, or input authority. On pause, offline, epoch replacement, unmount, or rejected protocol: hide/clear the Canvas, cancel/invalidate pending rendering, and require a new admitted complete frame. Async completion cannot reveal old content. A response/commit belongs to the exact admitted epoch and current status revision. Assets are immutable/versioned and protocol compatibility is explicit.

Local controller owns start/pause/resume/stop/source switching, credentials, selected output, and subprocesses. Commands must operate across separate CLI invocations. Stop closes the upload gate before remote acknowledgement; failed acknowledgement is reported distinctly and lease expiry handles unreachable relay. Pause continues heartbeat but sends no new content. Resume/switch invalidates queued output and sends a full snapshot. Fresh startup defaults offline; never silently publish all local web listeners. Private notes, unrevealed content, local controls, and diagnostic error strings are not audience output. Native relay failures must not break local rendering.

Full-frame v1 intentionally trades bandwidth for bounded, independently repairable delivery. Future negotiated exact-base deltas/resource deduplication may optimize it without changing compositor authority.
