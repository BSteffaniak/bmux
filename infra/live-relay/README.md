# BMUX live relay

Dependency-free Cloudflare Worker module: **`src/worker.js`**. Deploy as an ES
module (not a bundled service-worker script). Exports default `fetch` and named
`LiveChannel`. All remote/local HTTP state transitions run `ChannelHandler` from
this exact module. Production requires HTTPS; local adapter requires loopback.

## Infrastructure handoff

- `LIVE_CHANNELS`: Durable Object namespace binding, class `LiveChannel` in this
  Worker. Create the namespace using a **SQLite class migration** (`new_sqlite_classes`).
- `CHANNEL`: plain string, default `main`, matching `[a-z0-9][a-z0-9-]{0,47}`.
  Exactly one channel is allowlisted per deployment; unknown/deleted channels 404.
- `PUBLISHER_KEY_SHA256`: secret string, exactly 64 hexadecimal characters,
  SHA-256 digest of the channel's separately provisioned publisher bearer key.
  Never give the Worker the plaintext key or use deployment credentials as keys.
- Suggested compatibility date: `2026-10-14`. No Node compatibility flag required.
- Stable routes: `/api/live/{channel}` as defined in `docs/live-broadcast-protocol.md`.
  Viewer assets need a separate same-origin static deployment; this Worker handles
  only protocol endpoints and never grants CORS.

Persistence key `metadata` uses explicit version 1. It contains only UUID ownership,
lease/control state, revision, SHA-256 frame hash and fencing counter (under 1 KiB).
Frames are never written to Durable Object storage. Restart requires a fresh full
frame, retaining pause/revocation and revision identity. Unknown storage versions
fail closed. SQLite `put` acknowledgment precedes a successful mutation reply.
No alarm is needed: every access checks lease expiration; viewer reads recheck
before delivery, and pending streams are invalidated on pause/stop/replacement.
Bytes already delivered to a client cannot be recalled: viewer epoch/revision
checks and independent status polling remain mandatory.

Budgets per channel: one pending upload, 16 admitted requests, four viewer streams,
8 MiB upload/frame ceiling, 10 second body/delivery deadline, 64 KiB viewer chunks
with zero speculative queueing. Positional envelope admission bounds 262,144 cells,
2,097,152 total entries, 4,096 placements and 256 inline resources. Nested opaque
metadata and semantic image/glyph/render validation remain Rust authority. Admission
rejects deltas, missing full metadata/placements, unresolved inline resources,
truncation, trailing bytes, unknown versions and invalid headers. Current retained
frame plus four slow-viewer references plus upload is at most 48 MiB of frame/body
RAM (usually much less), excluding runtime overhead and small chunk/response buffers.

## Local development

Node 22+, no install required. `local.mjs` is executable and defaults to loopback
port 8787. Uses the same module, authentication and lifecycle, not a fake server.

```sh
export CHANNEL=main
# Supply a test publisher key separately to the native publisher or curl.
export PUBLISHER_KEY_SHA256=<64-character-test-key-digest>
node infra/live-relay/local.mjs --host 127.0.0.1 --port 8787 \
  --state-file /tmp/isolated-live-relay/metadata.json \
  --static-dir /absolute/path/to/built/viewer/dist
```

Both optional paths are explicitly supplied: no user runtime state is accessed.
Without `--state-file` authority metadata is process-local (use a file to test
restart). File mode is 0600; writes sync file and directory before acknowledgment.
Only one local adapter may own a state file. Without `--static-dir` no static files
are served. Static assets are a startup snapshot allowlist of conventional HTML,
JS, CSS, WASM, images and fonts, maximum 256 entries/32 MiB aggregate/16 MiB per
asset. Symlinks and traversal are rejected; no arbitrary filesystem or SPA/API
fallback. Viewer and API share one origin; CSP limits connections to that origin.

Example real workflow (use a Rust-produced self-contained BMWP2 snapshot):

```sh
base=http://127.0.0.1:8787/api/live/main
# $TEST_KEY contains the isolated test bearer, not a production credential.
curl --fail "$base/publisher/start" -H "Authorization: Bearer $TEST_KEY" \
  -H 'Content-Type: application/json' \
  --data '{"protocol":1,"renderer":"bmwp-2","publisher_id":"11111111-1111-4111-8111-111111111111"}'
# Set $EPOCH to the returned UUID, and $FIXTURE to actual encode_binary output.
curl --fail -X PUT "$base/publisher/frame" -H "Authorization: Bearer $TEST_KEY" \
  -H "X-Bmux-Epoch: $EPOCH" -H 'X-Bmux-Revision: 1' \
  -H 'Content-Type: application/octet-stream' --data-binary "@$FIXTURE"
curl --fail "$base/status"
curl --fail "$base/presentation?epoch=$EPOCH" -o /tmp/isolated-relay-frame.bmwp
cmp "$FIXTURE" /tmp/isolated-relay-frame.bmwp
```

Native clients must reject public plaintext publisher URLs, disable redirects,
set deadlines, gate uploads before pause/stop, and never allow transport failures
to break local rendering. Those protections are outside the relay's authority.

## Validation

```sh
cd infra/live-relay
npm test
# Coordinator supplies this path after generating a real Rust full snapshot:
BMWP2_FIXTURE=/absolute/path/to/actual-snapshot.bmwp npm test
```

Without `BMWP2_FIXTURE`, only the explicitly external cross-language test skips;
all offline state-machine and real HTTP adapter tests run. Tests allocate ephemeral
loopback ports and temporary isolated state, never a live daemon/runtime. The
external fixture test admits and uploads actual Rust bytes and compares the complete
binary response byte-for-byte. Run with plain and inline-image fixtures for coverage.
