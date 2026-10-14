# Presenter browser audience output

Presenter browser output uses the shared prepared TUI publisher and Rust/WASM
Canvas renderer. It is a read-only audience projection, not a browser terminal
or presenter control interface. Speaker notes, private status, and unrevealed
content must not enter the audience cells, image resources, selection, or
semantics. A local presenter can still see notes independently.

## Explicit opt-in

With matching native BMUX and bundled presenter plugin:

```sh
bmux present play deck.md --presenter --web
bmux present play deck.md --web-only
```

`--web` enables **mirror** mode without requiring prebuilt assets. The advanced
`--web-assets DIR` override also enables mirror mode: the local terminal remains
interactive and its audience viewport controls composition. Notes and local
controls are not mirrored. `--web-only` enables **only** mode without terminal
input, raw mode, or ANSI output. The initial audience viewport is 80×24; the first
browser can supply measured geometry. Additional browsers observe rather than
resize that view. Mirror browsers cannot replace local viewport authority.

Unscheduled web-only play holds a static read-only audience view until Ctrl-C.
There is no browser navigation and no automatic slide schedule. Local mirror
play uses ordinary terminal navigation. Source reload is distinct from slide
navigation.

`present host` accepts the same web options alongside its existing TLS and
credential options. For example, using existing private credential and TLS files:

```sh
bmux present host deck.md --listen 127.0.0.1:7443 \
  --cert /absolute/cert.pem --key /absolute/key.pem \
  --presenter-credential /absolute/presenter-credential --public-read \
  --web-only
```

The existing presenter-authorized host transport controls shared navigation and
custom-component actions. Browser viewing does not grant those permissions.
`--public-read` concerns host participants, not browser HTTP authentication.
`present join` and `present check` do not accept web-output flags.

## Configuration and precedence

Browser output defaults to off:

```toml
[presenter.web]
mode = "only" # off | mirror | only
assets = "/absolute/web-assets" # optional advanced override

[web]
port = 8080
host = "127.0.0.1"
auth = "token" # none | token
```

Mode selection is `--web-only`, then `--web` or explicit `--web-assets` (mirror),
then `BMUX_PRESENTER_WEB_MODE`, then `[presenter.web].mode`. Asset location is
explicit `--web-assets`, then `BMUX_WEB_ASSETS`, then `[presenter.web].assets`,
then implicit lazy setup. An explicit override is validated; it never triggers
installation or silently falls back to a different bundle.

Only enabled mirror/only output resolves assets. With no override, compatible
assets are reused from durable cache or prepared on first use. A populated
compatible cache supports offline use; a cache miss must complete setup before
any listener, raw mode, or attachment starts. Setup failure is reported, not
converted into partial web activation. The default off mode performs no browser
setup and local terminal operation remains independent. Source-checkout first
use builds with the pinned generator and caches the result; released binaries
resolve a pinned-tag bundle manifest. Older releases without a compatible
published bundle report an actionable error: upgrade or supply matching locally
built assets with `--web-assets DIR`, rather than substituting another version.
See [browser rendering](browser-rendering.md#attach-locally) for offline/cache
behavior. Cache precedence here is `BMUX_CACHE_DIR`, otherwise the native caller's
selected state directory joined with `cache`; the resolver adds `web-assets`.
Slot, named-runtime, and sandbox paths retain their own durable cache root,
without changing global environment or falling back to ordinary user state.

Merely setting configured assets, `BMUX_WEB_ASSETS`, `[web]`, or shared
`BMUX_WEB_*` network settings (including CLI network flags) does **not** enable
presenter web output.

Network settings use the same resolver as browser attach: CLI flags override
environment variables, which override `[web]` configuration:

| CLI | Environment | Configuration |
| --- | --- | --- |
| `--web-port` | `BMUX_WEB_PORT` | `port` |
| `--web-auth` | `BMUX_WEB_AUTH` | `auth` |
| `--web-host` | `BMUX_WEB_HOST` | `host` |
| `--web-public-host` | `BMUX_WEB_PUBLIC_HOST` | `public_host` |
| `--web-allowed-hosts` | `BMUX_WEB_ALLOWED_HOSTS` | `allowed_hosts` |

Host flags take IP addresses; allowed hosts are exact HTTP authorities, including
ports (for example `192.168.1.20:8080` or `[::1]:8080`), comma-separated on CLI/in
the environment and an array in TOML. Use explicit bind/public addresses for LAN or
IPv6. Do not publish a wildcard address as the audience URL. Authentication
`none` makes the reachable read-only audience public. Authentication `token`
requires the printed bootstrap URL; treat it as a capability and do not share
it in logs. Plain HTTP is not a confidential transport; use a trusted network
or a separately secured deployment. Rebuild native and WASM artifacts together
when their presentation protocol changes.

## Real-browser acceptance

After the coordinator grants the runtime/browser resource lock, run:

```sh
bash scripts/smoke-presenter-web.sh \
  --binary /absolute/coordinator-stage/bmux \
  --plugins /absolute/coordinator-stage/plugins \
  --assets /absolute/coordinator-stage/web-assets \
  --browser /absolute/chromium \
  --lan-address 192.168.1.20
```

Supply an actual local non-loopback IPv4 address. This runs a browser on the same
machine against the LAN bind; it does not prove reachability through another
machine's firewall. IPv6 runs when a kernel loopback bind is supported; unavailable
IPv6 is recorded explicitly, never counted as a passing IPv6 assertion.

The harness does not build, install, discover a binary on PATH, use a user daemon,
or borrow an existing browser profile. HOME, config, XDG paths, BMUX runtime,
state, data, logs, and browser profiles are isolated beneath a private retained
temporary directory. Native/browser/assets/plugin-library hashes accompany the
case report. Missing dependencies and required assertions fail rather than skip.

Cases require actual Canvas commits, PNG pixels, linked custom components,
unrevealed text/image privacy, local mirror versus browser audience, static
web-only play, authorized TLS host navigation/custom updates, first-browser
geometry and additional observers, none/token authentication, shared precedence,
LAN/IPv6 binding, rejection recovery, offline reconnect, and listener shutdown.
This is an acceptance specification, not a claim that execution has passed.
Acceptance remains pending until run against the coordinator's final artifacts.

The script imports existing CDP, HTTP, Canvas, and TLS protocol helpers read-only;
it does not run or modify the attach acceptance suite. Private fixtures contain
synthetic credentials and notes. Console and report output omit bootstrap URLs,
credentials, protocol payloads, and subprocess logs. Keep retained state private.
