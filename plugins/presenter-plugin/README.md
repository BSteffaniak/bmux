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

Mermaid runs through the embedded, bounded WASM renderer; no worker executable
or worker-path environment variable is required. The build automatically compiles
and embeds the guest, so release toolchains must include `wasm32-unknown-unknown`.
SVG rasterization uses the same bundled BMUX Diagram font as guest measurement,
including monochrome symbols/emoji, with installed system fonts for additional
script coverage. The bundled font is not a universal Unicode font. Mermaid
syntax/directive/timeout failures preserve local diagnostic text; source files
are never rewritten. Oversized diagrams scale into the bounded viewport.
Images and
diagrams are decoded/prepared outside update/paint; remote assets are opaque and
fetched in bounded chunks. Private audience credentials grant navigation/demo
input but never notes/effects; receive-only mode remains read-only.

## Playback modal

The presenter player centers a bounded retained modal with genuine TUI controls:
Play/Pause, Step, Stop, Restart, Close, and Half/Normal/Double/Instant speed.
Normal is the preparation default. Status reports the current action and action
count, not arbitrary seek progress. Close/Escape and slide navigation hide the
view without stopping its attempt. Removed references retain execution ownership;
compatible reinsertions reuse it, and whole-owner shutdown explicitly settles it.

Local and authenticated managed presenter clicks use committed clipped button
hits. Managed surface metadata is private to effects/demo-input authorized
principals; browser/audience rendering remains read-only without controls.

For explicit PTY test capture, set `BMUX_PRESENTER_FRAME_EVIDENCE` to an absolute
file path. Successful native presentation commits atomically replace a private
0600 JSON file with `revision`, `width`, `height`, rendered `rows`, clipped
`controls` (`id`, `input`, `x`, `y`, `width`, `height`, `enabled`), and `demo`
(`id`, `owner_epoch`, `generation`) or null. No evidence is written when unset;
publication errors are reported, not treated as successful capture. Speaker notes
are suppressed on the terminal while this explicit test evidence mode is enabled.

## Native TUI gallery

Markdown references an application-linked factory:

```markdown
<!-- bmux:block-id example-breadcrumbs -->
<!-- bmux:component tui-gallery {"kind":"breadcrumbs"} -->
```

The plugin registers `tui-gallery`, which returns real `bmux_tui_components`
controls. Rust snippets are never evaluated and no external process is started.
The existing `LinkedComponentFactory` extension point can return any application-
linked `Component`; unregistered names are not implicitly activated.

The tour includes 24 native examples. Tab focuses an example; Escape returns to
navigation. Button, checkbox, breadcrumbs, radio, list, tabs, tree, table, and text
input support keyboard input. Text input also accepts paste, capped at 4096 bytes.
Other fixtures are display-only, including the dropdown shown open. Viewer helpers
use real row-projection APIs; form validation is composed with native field controls.
State survives rebuilding; layout/paint never change canonical owner state.
Managed audiences use the existing read-only mirror, not another controller.

## Runnable examples

For Markdown playback, start with `packages/presenter/examples/quickstart.md`.
For the bundled image/Mermaid deck, run from the repository root in the same dev
shell and Cargo profile:

```sh
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
bmux --target local present play plugins/presenter-plugin/examples/live-demo.md --presenter --web
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
bmux present host plugins/presenter-plugin/examples/live-demo.md --managed --presenter-credential PRIVATE_FILE --public-read --no-reload
bmux present join bmux:HOST_ID --credential PRIVATE_FILE --presenter --mode shared
```

Only presenter effects authority may run/control the demo. `present check` and
joining an audience never authorize execution. Playbooks are executable input;
approve only trusted sources. Approval binds to the canonical deck root and
original source content; changed/new executable input is rejected. Compatible
reload retains the active attempt and frozen source; rejected executable changes
retain the last approved deck and run. Stop/Restart and exit acknowledge owned
cleanup. This is not a claim of crash recovery or arbitrary terminal-input support.

The lazy presenter-owned service keeps configuration, data, state, and diagnostics
under the selected `state/presenter-playbooks/service-*` directory. Its socket
runtime is a separately owned `bps-*` directory in the selected temporary root,
with Unix mode 0700, so long macOS `Library/Application Support` state paths do
not consume the Unix socket address budget. Successful acknowledged service
shutdown removes both directories; an unacknowledged exit preserves them for
diagnosis. Construction and inert shutdown allocate no socket directory. An
explicitly selected temporary root that is itself too long still fails clearly;
there is no silent host-runtime or alternate temporary-root fallback.

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

Bare `--broadcast` on `present play` or `present host` generates a fresh UUID,
registers the safe label `Presentation`, and publishes the configured default
profile. `--broadcast-profile NAME` selects exactly that named profile (and implies
broadcast); it never silently falls back to the default. Both use the existing
persistent proof-authorized connection, with no local web assets or HTTP listener.
Missing/invalid profiles and remote failures leave local playback usable and print
a content-free diagnostic directing the operator to `bmux broadcast doctor`.

```sh
bmux present play deck.md --broadcast
bmux present play deck.md --broadcast-profile staging
```

Publishing is attempted once per caller, not retried on reconnect: reconnect must
not reclaim a stream superseded by another publisher. Graceful teardown uses only
the private token returned by `start_profile` with `stop_owned`; stale tokens are
no-ops, never global stops. An unresponsive remote is fenced by controller leases.

Advanced `--broadcast SOURCE_UUID` on `present play` or `present host` registers only
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

Playbook demos are enabled by default for `present play` and `present host`.
Use `--no-playbooks` to opt out. Opening a deck remains inert: the presenter
must explicitly open and run a demo. Original-source approval and changed-source
rejection still apply; audience/join and check operations gain no execution
rights. The former `--allow-playbooks` flag remains a compatibility alias.
