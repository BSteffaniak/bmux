# TESTING.md

Testing guidance for bmux development, with emphasis on CLI/runtime and terminal protocol correctness.

## Quick Start

For most CLI/runtime changes:

```bash
cargo test -p bmux_cli
cargo check -p bmux_cli
./scripts/smoke-pty-runtime.sh
```

For terminal compatibility/protocol changes, also run:

```bash
./scripts/compat-matrix.sh
```

## Test Commands

### 1) Unit + integration tests (CLI crate)

```bash
cargo test -p bmux_cli
```

Covers:

- keymap/input parsing
- layout tree behavior
- runtime command handling
- protocol engine behavior (CSI/OSC/DCS, profile-gated replies)
- replay fixtures for fish/vim/fzf protocol sequences

### 2) Compile check (CLI crate)

```bash
cargo check -p bmux_cli
```

Ensures no compile regressions for the CLI/runtime path.

### 3) Runtime smoke tests

```bash
./scripts/smoke-pty-runtime.sh
```

Covers basic shell startup/interaction for:

- sh
- bash
- fish
- zsh
- keybind flow sanity

Expected output ends with:

- `smoke runtime checks passed`

### 4) End-to-end compatibility matrix

```bash
./scripts/compat-matrix.sh
```

Runs fish/vim/fzf scenarios across TERM/profile variants:

- bmux
- xterm
- screen
- conservative

Expected output ends with:

- `compatibility matrix checks passed`

Any `FAIL` line should be treated as a blocker for protocol/compatibility changes.

## When to Run What

### Always (for CLI/runtime code changes)

- `cargo test -p bmux_cli`
- `cargo check -p bmux_cli`
- `./scripts/smoke-pty-runtime.sh`

### Additionally for protocol/terminal compatibility changes

- `./scripts/compat-matrix.sh`

Examples:

- changes to query handling in `runtime/terminal_protocol.rs`
- changes to TERM/profile resolution in `runtime/mod.rs` or config behavior fields
- changes to pane output protocol handling in `runtime/pane_runtime.rs`

## Optional Manual Diagnostics

### Terminal doctor

```bash
bmux terminal doctor
bmux terminal doctor --json
bmux terminal doctor --trace --trace-limit 50
bmux terminal doctor --json --trace --trace-limit 50
```

Use trace mode when debugging protocol query/reply behavior (requires trace enabled in config).

### Terminfo install helper

```bash
./scripts/install-terminfo.sh
```

Useful when testing `bmux-256color` specifically.

## Failure Triage Hints

- Fish startup warning mentioning "Primary Device Attribute query"
  - Check protocol DA handling and profile mapping.
- Unexpected fallback to `xterm-256color`
  - Run terminal doctor and inspect terminfo checks.
- Compatibility matrix failures
  - Inspect scenario/profile row and reproduce using the same shell + pane TERM.

## Notes

- The compatibility matrix is heavier than smoke tests; use it whenever terminal protocol behavior changes.
- Keep protocol replay fixtures updated when intentional protocol responses change.
