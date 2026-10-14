# Presentation plugin

Optional plugin-owned `bmux present` commands:

- `present play deck.md [--presenter] [--no-reload]`
- `present check deck.md`
- `present host deck.md --presenter-credential FILE --audience-credential FILE --cert CERT --key KEY [--listen ADDRESS]`
- `present host deck.md --managed --presenter-credential FILE --public-read`
- `present join tls://HOST:PORT --ca CERT [--credential FILE] [--presenter] [--mode receive-only|submit-only|shared|independent]`
- `present join bmux:HOST_ID [--credential FILE] [--mode MODE]`

Credential files must be private (Unix mode 0600), containing 32–256 bytes.
TLS trust is explicit; no insecure or plaintext remote fallback exists. Public
read access requires `--public-read`. Presenter notes are separately authorized.
Linked Rust applications construct `Deck` with `DeckBuilder`, register trusted
factories in `LinkedRegistry`, and install `LinkedOwner::new(deck, assets, registry)`
in the same runtime `Host`; see `examples/linked_host.rs`. Compatible component
state survives atomic reload by block ID, component type, and state version.
This scope
excludes compiling `.rs` files through the CLI. JSON deck import is an optional
serialized-data adapter, not a Rust source-loading convention. Remote viewers
never execute Rust or Mermaid. Local reload retains the last valid deck on errors.

Mermaid requires the packaged `bmux-mermaid-worker` beside bmux, or an explicit
`BMUX_MERMAID_WORKER_PATH`. Worker identity is pinned before loading. Generic xtask auxiliary-executable
packaging builds/stages opted-in helpers on Unix and Windows (`.exe`). Native
Windows bundle and containment validation is still required; see the worker README.
Images and
diagrams are decoded/prepared outside update/paint; remote assets are opaque and
fetched in bounded chunks. Private audience credentials grant navigation/demo
input but never notes/effects; receive-only mode remains read-only.

## Runnable examples

For Markdown playback, start with `packages/presenter/examples/quickstart.md`.
For the bundled image/Mermaid deck, run from the repository root in the same dev
shell and Cargo profile:

```sh
cargo build -p bmux_mermaid_render --bin bmux-mermaid-worker
cargo run -p bmux_cli -- present check packages/presenter/examples/deck.md
cargo run -p bmux_cli -- present play packages/presenter/examples/deck.md
```

The deck includes its image and uses no custom components. See the presentation
crate README for helper discovery and image-terminal requirements.

For a visible interactive counter deck:

```sh
cargo run -p bmux_presenter_plugin --example counter_deck > counter.json
cargo run -p bmux_cli -- present play counter.json
```

Advance to the counter slide, use Tab to focus the counter, then `+`/`-` to change
it. The CLI registers its trusted demo components; JSON never supplies executable
code. `cargo run -p bmux_presenter_plugin --example linked_host` instead runs
a headless host/input/reload demonstration and prints confirmation; it does not
start a terminal player or a network listener.

The runtime owner renders fixed 80×24 mirror scenes; remote viewers never
reflow that canvas or instantiate the counter implementation.

Native playback uses `bmux_tui_runtime` scheduling, terminal input admission,
and image-aware presentation. Attach-keybinding dispatch refuses to acquire a
competing terminal. Navigation uses arrow keys/space, Home/End, and q to exit.

Only explicit subcommands are supported. `present` is a namespace, not an executable
leaf; `present deck.md` shorthand is intentionally unsupported because the registry
rejects a leaf that is also a parent of commands. No core parser special case is
introduced.
## Live playbook demo

Using a bundled BMUX executable that supports managed playbooks and the private
retained observer protocol:

```sh
bmux --target local present play plugins/presenter-plugin/examples/live-demo.md --allow-playbooks --presenter --web
```

Tab focuses `live-demo`; `o` opens the overlay, `r` manually starts the run,
`s` requests stop, and `R` restarts only after cleanup acknowledgment. `p`, `n`,
and `c` pause, single-step, and resume at script boundaries. Escape hides the
overlay without terminating its run. Leaving the slide requests stop. `--web` adds
a read-only browser audience showing the same fixed 80×24 retained output.
Browser viewport changes clip/center the display; they do not reflow the producer.
Local playback lazily starts its own private playbook service on `r`; no manual
server startup or runtime switching is needed. `--target local` is explicit here:
a selected remote target is not a local-execution shortcut. Managed hosts instead
require `bmux.playbook` on their selected server and never fall back to local
execution when that provider is missing.

Managed creation uses the explicit versioned approval contract:

```sh
bmux present host plugins/presenter-plugin/examples/live-demo.md --managed --allow-playbooks --presenter-credential PRIVATE_FILE --public-read --no-reload
bmux present join bmux:HOST_ID --credential PRIVATE_FILE --presenter --mode shared
```

Only presenter effects authority may run/control the demo. `present check` and
joining an audience never authorize execution. Playbooks are executable input;
approve only trusted sources. Approval binds to the canonical deck root and
original source content; changed/new executable input is rejected. Compatible
reload retains the active attempt and frozen source; rejected executable changes
retain the last approved deck and run. Stop/Restart and exit acknowledge owned
cleanup. This is not a claim of crash recovery or arbitrary terminal-input support.

### Isolated regression acceptance

Use explicit coordinator-built artifacts (no build or installation in the harness):

```sh
python3 scripts/presenter-owned-runtime-acceptance.py --binary /absolute/path/to/bmux --plugins /absolute/path/to/staged/plugins --assets /absolute/path/to/web-assets --browser /absolute/path/to/chromium
```

Linux `/proc` and Chromium are required. The harness owns all config, default/local
server, private runtimes, browser profiles and logs beneath a private temporary
root; it never attaches to the user daemon. It deliberately omits `bmux.playbook`
from the unrelated server, proves its principal/PID survives local playback, and
requires managed creation to fail clearly without a hidden local fallback. It
checks lazy startup, real shared shell output, compatible/rejected reloads, fresh
Restart generations and active-run `q` cleanup, then emits one capability-safe
summary pointing to retained private evidence. The existing coordinator managed
passing harness remains separate.

## Opt-in live audience source

`--broadcast SOURCE_UUID` on `present play` or `present host` registers only
that command's audience output. It does not start a relay broadcast or an HTTP
listener. Choose a stable UUID and select it separately:

```sh
bmux present play deck.md --broadcast 12345678-1234-4234-8234-123456789abc
# In another terminal (controller-owned credentials, never passed to presenter):
bmux broadcast start https://live.example.test main /private/publisher-key 12345678-1234-4234-8234-123456789abc
bmux broadcast pause
bmux broadcast resume
bmux broadcast stop
```

Existing `--web` mirrors and `--web-only` listeners can also use `--broadcast`.
Without a local listener, no assets are downloaded and no browser viewport lease
exists. Interactive playback retains terminal geometry authority; host audience
observation without a listener uses fixed 80×24 geometry. `--web-only` retains its
existing viewport behavior. Host still requires its normal transport/credential
flags (for example `--managed --presenter-credential FILE --public-read`). A
managed broadcast source keeps the caller running until Ctrl-C; the persistent
broadcast controller lives in the server plugin, not this caller process.

Only the shared compositor's audience projection enters full inline BMWP3
frames: private notes, speaker controls, and unrevealed material are excluded.
The output task retains only the latest frame, polls the source lease every
second, and republishes unchanged slides after controller refresh. It fences
async completion against selection/refresh and newer local frames. The 8 MiB
limit rejects oversized output rather than truncating it. Registration and
controller failures print content-free source status and do not stop local
playback; normal closure unregisters, connection cleanup/lease expiry covers
failure. Before every source registration/reconnect, presenter calls the generated
`prepare_local` service to prepare the server-owned proof, then reads the caller's
selected runtime directory's `broadcast-local-proof` and authorizes the same
connection through the generated control service. The proof must be an owned,
private, regular, single-link 32-byte file; unavailable/rotated/rejected proof leaves
local playback usable and retries without exposing proof bytes or filesystem
errors. Unix opens pin the owned runtime directory and reject symlinks; platforms
without this secure reader fail closed. A local source ID is not a publisher credential.
