# bmux slots

Slots let you install and run multiple bmux versions side-by-side with full
isolation: each slot owns its own binary, config, runtime dir (i.e. server
socket), data/state/log trees, and sessions. A client from one slot cannot
accidentally talk to a server from another.

> Status: foundation-pass. The slot system is opt-in and coexists with the
> legacy single-install mode. Packaging (apt/rpm/curl/npm) and a
> `cargo xtask install-slot` helper are follow-up work.

## Mental model

- A **slot** is a named, declaratively-described bmux install. Slots are
  enumerated in a single manifest: `~/.config/bmux/slots.toml` (or
  `$BMUX_SLOTS_MANIFEST`).
- Each slot installs as an explicit binary `bmux-<slot>`. There is no shim —
  the binary you invoke determines the slot.
- The manifest is the source of truth. `bmux slot` subcommands are read-only
  against it. Write-time refusal for declaratively-managed (read-only)
  manifests (e.g. Nix store) is built in.
- Slot identity is the slot's _name_. The version string is metadata.
- Layered config precedence (low → high):
  `~/.config/bmux/base.toml` → slot `bmux.toml` → `BMUX_CONFIG` env → `--config` flag.
  Set `inherit_base = false` on a slot (or `BMUX_NO_BASE_CONFIG=1` on an
  invocation) to skip the base layer entirely.

## Manifest — `slots.toml`

```toml
# Presentational default. Does not affect binary resolution (there is no
# shim); it only influences banners, `bmux slot list`, etc.
default = "stable"

# Optional: compose additional manifest files. Later files override earlier
# files at the slot-name level. Missing files are tolerated silently so you
# can have "optional local overrides" alongside a Nix-generated primary.
extend = [
  "~/.config/bmux/slots.local.toml",
]

[slots.stable]
binary       = "/usr/local/bin/bmux-stable"
inherit_base = true
# Optional explicit path overrides. When absent, defaults are derived under
# $BMUX_SLOTS_ROOT (or the platform data dir).
# config_dir  = "~/.config/bmux/slots/stable"
# runtime_dir = "${XDG_RUNTIME_DIR:-/tmp}/bmux/slots/stable"
# data_dir    = "~/.local/share/bmux/slots/stable"
# state_dir   = "~/.local/state/bmux/slots/stable"
# log_dir     = "~/.local/state/bmux/slots/stable/logs"

[slots.dev]
binary       = "${BMUX_DEV_BIN:-/home/you/GitHub/bmux/target/release/bmux}"
inherit_base = true
```

### Manifest grammar

- **Slot names** match `[A-Za-z0-9._-]+`. `default`, `current`, `all` are
  reserved.
- **Env interpolation** recognizes `${NAME}` and `${NAME:-default}`. Anything
  else (including `$var`) is preserved verbatim.
- **`~`** at the start of a path field expands to the user's home dir.
- **`extend`** entries are relative to the file containing them. Extend files
  are merged in order; later entries replace earlier at the slot-name level.
  Cycles and missing files are tolerated.
- **Duplicate `runtime_dir`** across slots is a hard error (each slot's
  runtime dir must be unique).

## Slot selection

Two ways, in precedence order:

1. **`BMUX_SLOT_NAME` env var** — overrides everything. Used by `bmux-env exec`
   and tests.
2. **argv[0] basename**: invoking `bmux-<slot>` selects that slot.

When neither is set, bmux falls back to legacy single-install behavior.

## Commands

The slot-management surface is reachable through three identical namespaces:

```
bmux slot <subcommand>        # primary
bmux env  <subcommand>        # alias for the above
bmux-env  <subcommand>        # standalone binary, same subcommands

# Read-only:
bmux slot list                       # all slots
bmux slot list --format json|nix     # machine-readable
bmux slot show [name]                # one slot's resolved detail
bmux slot paths [name]               # just the path grid
bmux slot doctor                     # validate manifest

# Write:
bmux slot install <name> <binary>    # register a slot
  [--no-inherit-base]                # do not layer ~/.config/bmux/base.toml
  [--mode symlink|copy]              # default symlink
  [--bin-dir <dir>]                  # default ~/.local/bin (or $BMUX_SLOTS_BIN_DIR)
  [--format toml|json|nix]           # for the printed block
  [--dry-run]                        # do not touch disk
bmux slot uninstall <name>           # remove slot
  [--purge]                          # also remove config/data/state/log dirs
  [--bin-dir <dir>]

# PATH / env bootstrapping:
bmux slot shell [--shell auto|bash|zsh|fish|nushell|powershell|posix]
bmux slot exec <name> -- <cmd> [args...]
bmux slot print [--format shell|json|nix|fish]
```

Every subcommand above works identically as `bmux env <same>` and as
`bmux-env <same>`. The `bmux-env` binary exists so that users can bootstrap
their `PATH` before any slot binary is reachable.

### Read-only manifest protection

`bmux slot install` and `bmux slot uninstall` refuse to mutate manifests
that are detected as "declaratively-managed" (e.g. under `/nix/store` or
`/etc`, or any prefix in `$BMUX_MANIFEST_READ_ONLY_PREFIXES`, or any file
with the read-only bit set). In that case:

- The would-be block is printed to stdout (`install`).
- A `note:` line explains the refusal.
- Exit code is `77`.

This makes the tool safe to run inside Nix / Home Manager workflows — users
can copy the printed block into their declarative config without any risk of
imperative drift.

## `bmux-env` helper

`bmux-env` is a standalone binary that exposes every subcommand above; it is
what you use in your shell rc to prepend the slot bin dir to `PATH`:

```
bmux-env shell [--shell auto|bash|zsh|fish|nushell|powershell|posix]
bmux-env exec <slot> -- <cmd> [args...]
bmux-env print [--format shell|json|nix|fish]
```

Examples:

```
# In ~/.zshrc (or equivalent):
eval "$(bmux-env shell)"

# Run cargo test against a specific slot's bin dir:
bmux-env exec dev -- cargo test

# Pipe the resolved env into Nix:
bmux-env print --format nix
```

`bmux-env shell`, `bmux-env exec`, and `bmux-env print` are pure — they only
emit to stdout (and `execvp` for `exec`). The install/uninstall subcommands
write, but they respect the same read-only-manifest refusal as the
`bmux slot` variants.

## Server isolation

When a slot is active, `server-meta.json` records the slot name. Both the
client (`bmux-<slot> ...`) and the server-startup path enforce:

- **Client-side**: attaching to a runtime dir whose server was started under
  a different slot is refused with a clear remediation hint.
- **Server-side**: starting a new server in a runtime dir that already hosts
  a live server under a different slot is refused.

## Nix composition

Because the manifest is a single declarative file, `bmux-env` is a pure
printer, and `bmux slot install` refuses to mutate read-only manifests, the
whole system composes cleanly with Nix. Example Home Manager shape (external,
informal):

```nix
home.file.".config/bmux/slots.toml".text = ''
  default = "stable"

  [slots.stable]
  binary = "${pkgs.bmux-stable}/bin/bmux-stable"

  [slots.dev]
  binary = "${pkgs.bmux-dev}/bin/bmux-dev"
'';
home.sessionPath = [ "$HOME/.local/bin" ];
```

Drop per-slot binaries into `~/.local/bin/` (or any dir on `PATH`) with whatever
naming strategy your package manager prefers — Nix users can `runCommand` a
tiny wrapper derivation that symlinks the bmux binary as `bmux-<slot>`.

A first-class flake / Home Manager module is planned as a follow-up.

## Environment variables

All new and relevant variables (see `bmux doctor env` or `bmux slot list`):

| Variable                           | Scope   | Purpose                                                           |
| ---------------------------------- | ------- | ----------------------------------------------------------------- |
| `BMUX_SLOT_NAME`                   | slot    | Forces the active slot; overrides argv[0].                        |
| `BMUX_SLOTS_MANIFEST`              | slot    | Path to `slots.toml`; `-` reads stdin.                            |
| `BMUX_SLOTS_ROOT`                  | slot    | Root under which per-slot default dirs materialize.               |
| `BMUX_SLOTS_BIN_DIR`               | slot    | Dir containing `bmux-<slot>` binaries. Default `~/.local/bin`.    |
| `BMUX_NO_BASE_CONFIG`              | config  | When truthy, skips the shared `base.toml` layer.                  |
| `BMUX_MANIFEST_READ_ONLY_PREFIXES` | slot    | Colon-separated prefixes treated as read-only.                    |
| `BMUX_SLOT_BANNER`                 | runtime | Set to `off` to suppress the interactive non-default-slot banner. |

Each slot's `BMUX_*_DIR` env vars (`BMUX_CONFIG_DIR`, `BMUX_RUNTIME_DIR`,
`BMUX_DATA_DIR`, `BMUX_STATE_DIR`, `BMUX_LOG_DIR`) still override paths at
highest precedence — useful for sandbox/test flows.

## What is NOT in this pass

- No cutover: `bmux` (bare) still works as legacy single-install. A future
  pass renames to `bmux-<slot>` across apt/rpm/curl/npm and ships a `default`
  symlink.
- No first-class Nix flake / Home Manager module (design is ready; external
  authors can build one against the public surface today).
- No config schema-version field / migrations.

## Quickstart: "spin up a dev slot for my current checkout"

```
# One-time: put ~/.local/bin on PATH (many distros already do).
eval "$(bmux-env shell)"

# From anywhere:
bmux slot install cursor /home/you/GitHub/bmux/target/release/bmux \
    --no-inherit-base

# Verify:
bmux slot doctor
bmux slot paths cursor      # distinct runtime_dir from any other install

# Run an isolated bmux:
bmux-cursor                 # opens a fresh server under the 'cursor' slot

# Iterate: cargo build --release in the repo, the symlink keeps pointing
# at target/release/bmux so the next `bmux-cursor` invocation picks up the
# new binary automatically.

# Remove:
bmux slot uninstall cursor [--purge]
```
