# Live broadcasting protocol v1

BMUX live broadcasting reuses self-contained `BMWP\x05` full presentations. This is an output-only transport, not terminal input, navigation, or viewport control. Local operation never depends on the relay.

## Change-driven delivery and optional compression

Presenter timer wakeups check for state changes; they are not an instruction to
publish a frame. Audience publications deduplicate against the last admitted
content, ignoring generated revisions and damage hints but preserving generation,
lifecycle, provenance, and interaction metadata. Local notes/focus do not invalidate
audience content. Retained full state remains available for reconnect and repair.

The local web host explicitly negotiates `Accept-Encoding: gzip` for JSON,
document, full-frame and delta responses. Compression is cached once per
publication, only when smaller, with a 1 KiB threshold, 8 MiB aggregate input-work
budget and 2 MiB additional retained compressed-byte budget. PNG resources remain
identity encoded. `/diagnostics` exposes identity/transport byte and representation
counters without writing into the terminal presentation.

Internet broadcasts default to self-contained full frames, suppressing unchanged
content and coalescing pending work. Optional relay deployment setting
`DELTA_BUNDLES=fanout` enables explicitly negotiated `bmwp-delta-v1` delivery.
The publisher sends a full repair snapshot plus a smaller delta in one bounded
bundle; **this increases publisher uplink bytes**, and is appropriate only when
viewer fan-out savings justify the cost. There is no delta-only relay materializer.
Legacy publishers/viewers remain full-frame compatible.

Negotiated viewers supply exact generation/base revision and relay cursor/epoch;
only a matching base receives a delta. Reconnect, missing base, epoch/source
transition or presentation failure falls back to the retained full snapshot.
A stateful hidden Canvas decoder retains the base; visible output is copied only
after successful admission. `X-Bmux-Compression: gzip-v1` separately negotiates
smaller relay payloads with independent 8 MiB encoded/decoded limits. Relay
counters expose full/delta/repair counts, upload/full-equivalent/bundle overhead
bytes and viewer wire bytes. Heartbeats and status polling remain necessary
control traffic, not content changes.

## Coordinated renderer upgrade

Renderer negotiation is now `bmwp-5` (`BMWP\x05`); the HTTP protocol remains v1. Publishers, relay admission, and the immutable viewer bundle must be deployed together. Stop active publishing before upgrading the relay, rebuild and deploy the matching viewer assets, then restart publishers. Earlier renderer negotiation and binary v1–v4 frames are rejected, not reinterpreted. This is not a rolling or dual-version upgrade. Authentication, epoch fencing, revision ordering, and leases remain unchanged.

## HTTP contract

All routes are under `/api/live/{channel}`. Channels match `[a-z0-9][a-z0-9-]{0,47}`. No CORS. Production requires HTTPS; loopback HTTP is allowed for local development. Errors are content-free JSON. No bearer, request bodies, terminal content, or publisher IDs may appear in logs.

Publisher routes require `Authorization: Bearer <channel-publisher-key>`. Keys are provisioned separately from deployment credentials. Relay configuration initially maps one configured channel (`main` by default) to a SHA-256 publisher-key digest (`PUBLISHER_KEY_SHA256`), never plaintext. Requests other than start additionally require `X-Bmux-Epoch: <epoch>`.

- `POST /publisher/start`: JSON `{protocol:1,renderer:"bmwp-5",publisher_id:"<UUID>"}`. Returns 200 JSON `{protocol:1,renderer:"bmwp-5",epoch:"<UUID>",lease_ms:15000}`. Creates `starting`; rejects another unexpired owner with 409. Exact retries by the same private publisher ID return the same epoch without undoing pause. Unsupported protocol/renderer is rejected. Expired ownership cannot be revived by an old epoch. No implicit takeover.
- `PUT /publisher/frame`: self-contained full BMWP v3 bytes, `Content-Type: application/octet-stream`, `X-Bmux-Revision: <positive decimal u64>`. Maximum 8 MiB (8,388,608 bytes). Monotonically advancing within an epoch; exact revision+bytes retry is idempotent; conflicting/stale revisions rejected. Valid header/full binary envelope admission is required before publication; complete Rust validation remains the producer/viewer authority. Commit atomically replaces the frame, moves starting/live to live, and renews lease. Reject while paused. Reply 204.
- `POST /publisher/heartbeat`: renew live/starting/paused lease; reply 204. Heartbeat never changes paused/starting to live.
- `POST /publisher/pause`: clear public frame and mark paused; reply 204. In-flight pre-pause frames must not repaint after this transition. Native publisher gates content before uploading and serializes mutations against pause.
- `POST /publisher/resume`: only paused -> starting, frame absent; fresh full frame required for live. Reply 204.
- `POST /publisher/stop`: revoke epoch, clear public frame, offline; reply 204. Idempotent retry for the revoked epoch is allowed without affecting any replacement epoch.

Relay must fence stale requests, enforce actual lease expiry on every access, and bound pending uploads, retained bytes, clients, and body reads. Persist enough lease/epoch/control state to prevent restart undoing pause or resurrecting stopped publishers. Frames may be volatile: after relay restart require a fresh full frame and report starting, not live. Reject deleted/unknown channels. Native requests have deadlines and redirects disabled. Do not log credential URLs or raw transport errors.

Viewer routes require no credential:

- `GET /status`: JSON `{protocol:1,renderer:"bmwp-5",state:"offline|starting|live|paused",epoch:null|"<UUID>",revision:"0"|"<u64>",lease_expires_at:null|<Unix milliseconds>}`. Expired becomes offline, not a frozen live frame. `revision` is the relay delivery cursor, independent of the source revision embedded in BMWP. Cache-Control no-store.
- `GET /presentation?epoch=<epoch>&after=<revision>`: 200 application/octet-stream full current frame with `X-Bmux-Epoch` and `X-Snapshot-Revision`; 204 if same cursor; 409 if epoch mismatches; 503 when not live/no frame. `after` is optional. May return immediately; viewer polls at bounded interval (initially 1 second) with independent status polling to promptly hide paused/offline content. No resource routes are needed for self-contained presentations.
- `GET /health`: content-free versioned relay health; no secrets.

## Viewer and local control

Viewer uses the existing Rust/WASM Canvas renderer with inline resources and validates all bytes. Public viewport is fixed by producer; viewer scales/letterboxes. No `/view`, `/viewport`, keyboard, navigation, or input authority. On pause, offline, epoch replacement, unmount, or rejected protocol: hide/clear the Canvas, cancel/invalidate pending rendering, and require a new admitted complete frame. Async completion cannot reveal old content. A response/commit belongs to the exact admitted epoch and current status revision. Assets are immutable/versioned and protocol compatibility is explicit.

Local controller owns start/pause/resume/stop/source switching, credentials, selected output, and subprocesses. Commands must operate across separate CLI invocations. Stop closes the upload gate before remote acknowledgement; failed acknowledgement is reported distinctly and lease expiry handles unreachable relay. Pause continues heartbeat but sends no new content. Resume/switch invalidates queued output and sends a full snapshot. Fresh startup defaults offline; never silently publish all local web listeners. Private notes, unrevealed content, local controls, and diagnostic error strings are not audience output. Native relay failures must not break local rendering.

Full-frame v1 intentionally trades bandwidth for bounded, independently repairable delivery. Future negotiated exact-base deltas/resource deduplication may optimize it without changing compositor authority.
