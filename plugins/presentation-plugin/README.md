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

For Markdown playback, start with `packages/presentation/examples/quickstart.md`.
For the bundled image/Mermaid deck, run from the repository root in the same dev
shell and Cargo profile:

```sh
cargo build -p bmux_mermaid_render --bin bmux-mermaid-worker
cargo run -p bmux_cli -- present check packages/presentation/examples/deck.md
cargo run -p bmux_cli -- present play packages/presentation/examples/deck.md
```

The deck includes its image and uses no custom components. See the presentation
crate README for helper discovery and image-terminal requirements.

For a visible interactive counter deck:

```sh
cargo run -p bmux_presentation_plugin --example counter_deck > counter.json
cargo run -p bmux_cli -- present play counter.json
```

Advance to the counter slide, use Tab to focus the counter, then `+`/`-` to change
it. The CLI registers its trusted demo components; JSON never supplies executable
code. `cargo run -p bmux_presentation_plugin --example linked_host` instead runs
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
introduced. Managed host lifetime belongs to BMUX, not the CLI.
