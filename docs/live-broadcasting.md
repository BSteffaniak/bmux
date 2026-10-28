# Live broadcasting

Live is opt-in, output-only publication of one explicitly selected authoritative
retained presentation. Local rendering remains independent. The audience cannot
type, navigate, resize the source, or access private controller state. See the
[Rust streaming protocol](live-stream-protocol.md) and
[Fly deployment and recovery](live-deployment.md).

## Native control interface

The broadcast plugin is server-owned. Separate invocations address the same
controller. Start, pause, resume, stop, and explicit source switching are
runtime-wide
operator actions authorized by the selected runtime's private local proof, not
by the short-lived CLI connection owning the source. Starting an existing source
(`broadcast start --source UUID`, optionally with `--profile NAME`, or the
positional form below) preserves its producer's ownership and requires a live
registration. It cannot replace an active publication. Integrations retain
owner-scoped controls and token-fenced `stop-owned` cleanup:

```sh
bmux broadcast start https://live.bmux.dev main /PRIVATE/PATH/publisher-key SOURCE_UUID
bmux broadcast status
bmux broadcast pause
bmux broadcast resume
bmux broadcast source ANOTHER_SOURCE_UUID
bmux broadcast stop
```

Start arguments are **positional**, not flags. Configure the HTTPS origin, not a
WebSocket route: the native publisher handles authenticated control and WSS
stream establishment. `main` remains the native single-channel selection.
Publisher credentials are owned private regular files, never command-line key
values, URLs, logs, configuration contents or public CI artifacts.

A presenter explicitly launched with `--broadcast SOURCE_UUID` registers output
through its persistent authenticated connection; terminal-source selection uses
its own native CLI interface. Do not confuse a source UUID with a lease epoch,
fence or publisher identity. Do not silently publish arbitrary local listeners.
Production endpoints require HTTPS/WSS; loopback development must be explicitly
isolated and must not weaken production authentication or origin validation.

Pause clears public content while retaining authority through lease renewal.
Resume and source switch require a complete full frame. Stop closes the local
upload gate even if remote acknowledgement fails; report remote uncertainty and
let lease expiry fence unreachable ownership. A fresh local runtime defaults
offline. Local status is not proof of public delivery.

## Rust streaming and privacy

One always-on Fly Machine runs `bmux-live-relay`. The viewer is served by Pages
at `https://bmux.dev` and connects to
`wss://live.bmux.dev/v1/viewer/ws` with an exact allowed browser origin. Native
publisher traffic uses authenticated `/v1/publisher/*` control and WebSocket
routes; never put keys into WebSocket URLs.

Protocol 1 carries BMWP version 5 (`bmwp-5`) full frames and deltas over
persistent
WebSockets. Connection and repair start from a complete full frame. A slow
viewer repairs from bounded current state rather than accumulating an unbounded
queue. Idle streams do not poll status or repeatedly upload snapshots. There is
no JS Worker compatibility backend. Use the protocol document for exact
framing, authority fencing, limits and failure semantics.

Frames remain bounded RAM-only data, not durable history. The encrypted volume
contains only private authority metadata at `/data/relay/authority.json`; state
backups are not recording archives. The service is output-only, not a remote
terminal control plane. A private publisher key is separate from the metrics
key. Neither belongs in presentation frames or public diagnostics.

## Isolated development acceptance

The obsolete JS `live-dev.mjs` launcher has been removed. Use the owned Rust/TLS
harness with explicit artifacts. The following builds are for a deliberately
reserved local development slot, not commands implicitly run by deployment:

```sh
nix develop --command bash -euo pipefail -c '
  cargo build --locked -p bmux-live-relay
  cargo build --locked -p bmux_web_renderer --example snapshot --example wire_frame
  target_dir="$(cargo metadata --locked --no-deps --format-version 1 | python3 -c "import json,sys; print(json.load(sys.stdin)[\"target_directory\"])")"
  BMUX_WEB_ASSET_OUTPUT=/absolute/new/viewer bash scripts/build-web-renderer.sh
  "$target_dir/debug/examples/snapshot" --binary > /absolute/private/native-frame.bmwp
  python3 scripts/smoke-live-local.py --relay "$target_dir/debug/bmux-live-relay" \
    --assets /absolute/new/viewer --frame /absolute/private/native-frame.bmwp
'
```

Supply new owned output directories and protect frame fixtures as private data.
The harness runs an explicitly supplied Rust executable against isolated local
TLS endpoints and temporary authority state, and verifies generated immutable
asset integrity. It is not a persistent development backend or production
configuration. It must not attach to or stop an existing user relay.

Browser acceptance takes explicit `--browser`, `--site`, `--fixture`, `--relay`
and `--wire-tool` paths. Native acceptance takes explicit `--binary`,
`--sha256`,
`--assets`, `--browser`, `--relay` and `--wire-tool`. Use the pinned-built
`debug/examples/wire_frame` from Cargo's resolved target directory, not a
guessed
`target/debug` path. These additional harnesses establish browser/native
behavior;
the local smoke alone does not prove all interactive or visual privacy races.

## Production checks and observability limits

### Diagnose a frozen broadcast

On builds supporting `broadcast-diagnostics-v1`, use the same selected runtime:

```sh
bmux broadcast status --verbose
bmux broadcast diagnose --json
bmux broadcast events --last 50
```

These commands are local read-only observations: they do not restart a stream,
renew a lease, publish a test frame, or grant authority. Histories are bounded
and process-local. Capture them before restarting the process. Older loaded
plugins may not support the service even when the CLI is newer.

`doctor` checks local profile/credential-file permissions and public relay
health; it does not test publisher authentication, frame acknowledgments or
viewer rendering. A retained frame and a healthy relay are not proof of ongoing
updates. `idle` means there is no observed pending work; old content alone does
not make an unchanged terminal stalled. Inspect pending/send/ACK ages and the
current versus last failure. Correlate revisions only within their generation
and use monotonic elapsed ages, not subtraction of clocks on different hosts.

The viewer's **Delivery diagnostics** panel exposes a sanitized local export.
Receive, decode, renderer submission and ACK-send are separate milestones;
renderer submission is not proof of physical screen display, and ACK-send is
not proof the relay received that ACK. No terminal content is included.

Operators can query `/v1/diagnostics` with the separate metrics credential and
`X-Bmux-Diagnostics-Version: 1` for relay connection, repair and admission
observations. Keep the credential in a private client configuration rather than
command text. See `packages/live-relay/OBSERVABILITY.md` for the diagnostic
contract. Missing diagnostic support is unknown, not a passing check.

Public smoke is read-only and keeps site and relay origins separate:

```sh
python3 scripts/smoke-live-broadcast.py --url https://bmux.dev --relay-url https://live.bmux.dev
```

It probes relay `/health` for protocol 1 / `bmwp-5`, discovers immutable site
assets from `/live`, checks JS/WASM sizes and SHA-256 digests, and recomputes
bundle
identity. It does not poll legacy status endpoints, publish frames, use
publisher
credentials, print remote bodies, or follow redirects. This is not visual or
volume-persistence acceptance.

`/metrics` requires an independent metrics bearer key and exposes content-free
Prometheus data. No authenticated scraper, durable metrics retention,
dashboard, or alerting service is provisioned by the relay deployment itself.
An optional operator-managed [private monitoring stack](../infra/live-monitoring/README.md)
provides Prometheus retention, Grafana provisioning and Alertmanager webhook
configuration; it must be deployed and verified separately. The endpoint alone
is not persistent observability; counters lost on process restart are not an
incident history. Never store terminal payloads, bearer keys or
publisher identity in metric labels or diagnostic artifacts.

Offline infrastructure checks (no builds or deployment):

```sh
python3 scripts/test-live-build-artifact.py
python3 scripts/test-live-fly.py
python3 scripts/test-live-deployment-boundary.py
python3 scripts/test-live-smoke.py
bash -n scripts/live-deploy.sh scripts/build-live-artifact.sh
# Provider initialization downloads pinned providers; it does not contact live state.
tofu -chdir=infra/live-terraform init -backend=false -input=false -lockfile=readonly
tofu -chdir=infra/live-terraform fmt -check
tofu -chdir=infra/live-terraform validate
```

Approval, credentials, immutable image publication and
canonical encrypted R2 state recovery remain separate operator workflows. See
the deployment document before any production action.
