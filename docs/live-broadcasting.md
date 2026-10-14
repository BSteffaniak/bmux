# Live broadcasting

Live is an opt-in, output-only publication of one explicitly selected authoritative retained presentation. Local rendering remains independent. The audience cannot type, navigate, resize the source, or access private controller state. See [protocol v1](live-broadcast-protocol.md) and [deployment](live-deployment.md).

## Native control interface

The broadcast plugin is server-owned. Separate invocations address the same controller:

```sh
bmux broadcast start https://YOUR-LIVE-HOST main /PRIVATE/PATH/publisher-key SOURCE_UUID
bmux broadcast status
bmux broadcast pause
bmux broadcast resume
bmux broadcast source ANOTHER_SOURCE_UUID
bmux broadcast stop
```

The start arguments are **positional**, not flags. JSON reports `{local,remote,source_id,error_code}`; remote failure is not proof the upload gate is open. A presenter explicitly launched with `--broadcast SOURCE_UUID` registers output through its persistent authenticated connection; terminal-source selection follows its own native CLI interface. Do not confuse a source UUID with a channel, lease epoch, or public publisher identity. Do not silently publish arbitrary local web listeners. Remote endpoint must be HTTPS, except loopback HTTP development. Publisher credential is an owned private regular file; no plaintext credential in CLI arguments, logs, config, or CI.

Pause hides/clears public content while retaining the owner via heartbeat. Resume/source switch requires a new complete full frame. Stop closes the local upload gate even if acknowledgement fails; report remote uncertainty and let relay lease expiry fence unreachable ownership. Starting a fresh local runtime defaults offline. Frames are bounded full BMWP v2 snapshots; selected compositor output remains authoritative, not an independent mirror.

## Isolated local stack using real Node relay and real WASM

Build only after a build reservation (the coordinator owns workspace validation). The explicit web build uses Nix-selected Rust and stages a SHA-256-named immutable audience bundle before docs compilation:

```sh
bash scripts/dev.sh bash scripts/build-web-renderer.sh
VERSION=$(cat packages/docs-site/public/live-assets/current-version)
ASSETS="$PWD/packages/docs-site/public/live-assets/$VERSION"
```

Use a fresh private directory and ephemeral ports; **never restart or attach a live user daemon**:

```sh
umask 077
LOCAL=$(mktemp -d)
python3 -c 'import secrets; print(secrets.token_urlsafe(48))' > "$LOCAL/publisher-key"
node scripts/live-dev.mjs --assets "$ASSETS" \
  --key-file "$LOCAL/publisher-key" --state-file "$LOCAL/relay.json" \
  --ready-file "$LOCAL/ready.json"
```

The command prints a loopback viewer URL. `ready.json` contains only the local origin and explicit protocol/renderer, never the key. The proxy serves `/live` and immutable `/live-assets/<SHA256>/` with the same `ChannelHandler`/Worker module as production via `infra/live-relay/local.mjs`, not a fake implementation. State contains small durable authority metadata, frames remain bounded RAM-only. Stop this **owned process** with SIGTERM; it closes only its own servers. The real Canvas viewer uses `/api/live/main` same-origin; no permissive CORS.

For native integration isolate **all four** ConfigPaths directories (`BMUX_CONFIG_DIR`, `BMUX_RUNTIME_DIR`, `BMUX_DATA_DIR`, `BMUX_STATE_DIR`), `HOME`, every XDG config/data/state/runtime path, and recordings. Clear inherited runtime/slot/config/socket overrides; disable base config. Use a distinct explicit built `bmux` executable whose hash is recorded. Start only a server/presenter using those paths, register the chosen `SOURCE_UUID`, and run the positional native commands above against the origin in `ready.json`. Do not run them until native startup/source registration interfaces are verified; environment isolation alone cannot prove a command honored the intended runtime. Inspect the selected paths/socket before publishing.

## Runnable native-frame/relay/WASM HTTP acceptance

This focused command creates its own key/state/ports/process, publishes an actual native Rust frame through the same Node relay, verifies delivered bytes, tests pause/resume/stop visibility, validates public compatibility and every hashed JS/WASM asset, then terminates the owned stack:

```sh
bash scripts/dev.sh sh -c 'cargo run --locked --quiet -p bmux_web_renderer --example snapshot -- --binary > "$1"' sh /PRIVATE/PATH/frame.bmwp
python3 scripts/smoke-live-local.py --assets "$ASSETS" --frame /PRIVATE/PATH/frame.bmwp
```

`--binary` uses the actual retained presentation renderer, not handcrafted magic-only bytes. This tests HTTP admission/delivery and real generated asset integrity. **It does not claim native controller integration, browser Canvas rendering, terminal-source selection, or visual privacy-race proof.** Coordinator-owned native/browser acceptance must cover those separately before end-to-end acceptance. No cargo/browser validation was run by the infra lane without reservation.

Public deployment smoke is read-only:

```sh
python3 scripts/smoke-live-broadcast.py --url https://YOUR-LIVE-HOST
```

It discovers the immutable bundle from `/live` HTML, verifies manifest schema/protocol/renderer, JS/WASM sizes and SHA-256 digests and recomputes bundle identity. It never requests presentation bytes, uses a publisher credential, logs remote response content, or follows redirects. Health/status and all asset requests have explicit deadlines/bounds. Public smoke is a deployment check, not a visual renderer assertion.

Offline infrastructure checks:

```sh
python3 scripts/test-live-bootstrap.py
python3 scripts/test-live-state.py
node --check scripts/live-dev.mjs
bash -n scripts/live-deploy.sh scripts/build-live-artifact.sh
tofu -chdir=infra/live-terraform init -backend=false -input=false -lockfile=readonly
tofu -chdir=infra/live-terraform fmt -check -recursive
tofu -chdir=infra/live-terraform validate
```

No commands above bootstrap live secrets or apply/deploy infrastructure implicitly. Credentialed mutations are explicitly separated in deployment documentation.
