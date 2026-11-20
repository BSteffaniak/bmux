# CLI ownership reference

Run with Python 3.11+ from the repository root. No workspace build is performed
by these commands:

```sh
python3 -B scripts/cli-ownership/ownership.py check
for mode in bare local-splits default disabled; do
  binmode=$mode; [ "$mode" != disabled ] || binmode=default
  python3 -B scripts/cli-ownership/compat.py check "$mode" "/path/to/builds/$binmode/bmux" || exit 1
done
```

`paths.json` is the independent source inventory (`ownership.py baseline`, using
`git show` on 3320b28b58b6); `ownership.py map` reads the current checkout
instead. Claims, declarations (including aliases), static parser paths, core
execution rows and known inbound core bridges are separate fields. `check`
detects dropped paths, ambiguous declarations, stale static leaves, known bridge
destinations without a backing core row, new bridge files and added host
methods. Passing does **not** prove all domain code has moved, that a plugin is
enabled, that it implements its handler correctly, or that arbitrary subprocess
invocations are safe. Review changed Rust table/derive/generator forms alongside
this source reader.

The gzip JSON fixtures contain `[exit, base64(stdout), base64(stderr)]` keyed by
argv (all cases have space-free tokens). Each invocation gets empty stdin and a
fresh private home, config, data, runtime and state directory. Normalization
replaces that directory; for pre-existing Clap debug assertion panics only, it
replaces the thread ID and machine-specific registry path prefix. Nothing sorts
or merges the two streams. The disabled profile uses the default binary with all
baseline bundled plugin IDs explicitly disabled. Interpret profile deltas
individually: an intended plugin-absent removal should be reviewed, not blessed
by regenerating the fixture from the changed tree.

To reproduce the references, unpack `git archive 3320b28b58b6` into a temporary
directory. In the repository Nix environment use an isolated archive-local
target directory, `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0`, and `cargo
build --locked --offline -p bmux_cli --bin bmux`. Preserve a copy named `bmux`
for each variant before rebuilding: `bare` uses `--no-default-features`;
`local-splits` additionally uses `--features
bundled-plugin-pane-runtime,bundled-plugin-windows,bundled-plugin-sessions,bundled-plugin-contexts,bundled-plugin-clients`;
default uses no feature flags. Use `compat.py capture MODE BINARY --fixtures
/tmp/new-empty-directory` only with these baseline binaries. Captures made here
were on macOS arm64; use same-platform and same-debug-assertion settings or
capture a separate baseline on the target platform.

`migrated.json` pins each removed leaf to its intended plugin. `ownership.py
check` verifies explicit manifest CLI exposure, canonical or alias path,
matching native callback registration, and no residual core execution row for
these leaves. The source-side registration reader handles literal and
literal-array `blocking_command` registrations; add a verified reader if a
plugin switches styles. `python3 -B -m unittest discover -s
scripts/cli-ownership` exercises hidden and dangling commands and verifies
disposition strictness.

`compat.py check` uses the frozen bytes by default. `dispositions.py` checks an
exact unknown-command transcript when an explicitly listed owner is absent; even
restoring the old core fallback is a failure. For affected parent help only, it
removes those named children from the frozen Commands list and compares
remaining text, order and both channels. The explicit default `logs profiles
list --json` disposition requires successful local output instead of the legacy
proxy error. No default missing-command or blanket help differences are
accepted.

These are command-surface, validation and selected local execution checks, not
PTY, live daemon, deployment, clipboard, QR, or authentication integration
tests. Eight default-profile help probes already panic in the debug baseline on
conflicting cluster flags. Their original diagnostics and exit codes are kept,
not treated as successful help.
