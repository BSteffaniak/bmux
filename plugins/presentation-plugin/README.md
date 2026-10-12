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
Linked Rust applications construct `Deck` with `DeckBuilder`, instantiate `Host`,
and install a trusted runtime `Owner`; see `examples/linked_host.rs`. This scope
excludes compiling `.rs` files through the CLI. JSON deck import is an optional
serialized-data adapter, not a Rust source-loading convention. Remote viewers
never execute Rust or Mermaid. Local reload retains the last valid deck on errors.

Mermaid requires the packaged `bmux-mermaid-worker` beside bmux, or an explicit
`BMUX_MERMAID_WORKER_PATH`. Worker identity is pinned before loading. Generic xtask auxiliary-executable
packaging installs it on Unix; the current helper packaging path excludes Windows,
so Windows installations do not yet provide bundled Mermaid rendering. Images and
diagrams are decoded/prepared outside update/paint; remote assets are opaque and
fetched in bounded chunks. Private audience credentials grant navigation/demo
input but never notes/effects; receive-only mode remains read-only.

`examples/counter_deck.rs` exports a serialized linked counter fixture for local
CLI playback. The runtime owner renders fixed 80×24 mirror scenes; viewers never
reflow that canvas or instantiate the counter implementation.

Native playback uses `bmux_tui_runtime` scheduling, terminal input admission,
and image-aware presentation. Attach-keybinding dispatch refuses to acquire a
competing terminal. Navigation uses arrow keys/space, Home/End, and q to exit.

Only explicit subcommands are supported. `present` is a namespace, not an executable
leaf; `present deck.md` shorthand is intentionally unsupported because the registry
rejects a leaf that is also a parent of commands. No core parser special case is
introduced. Managed host lifetime belongs to BMUX, not the CLI.
