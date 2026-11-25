# CLI workflows

For reaching this machine's sessions from another device, see
[Remote access and pairing](remote-pairing.md).

The current CLI is server-backed by default. Running `bmux` with no subcommand
starts or reuses a server, creates a session when needed, and attaches.

`bmux server stop` waits for the selected local process to exit, timing out
after
30 seconds without observable shutdown progress (not 30 seconds total).
Completed
cleanup milestones advance an instance-bound status counter; repeated status
reads
and waiting diagnostics do not extend the wait. The last known step is included
on inactivity timeout. Older servers without progress retain a bounded wait.
Graceful completion requires an instance-matching cleanup record; an older
server
without that record is reported as stopped with cleanup unverified. Cleanup
errors
are failures, not successful stops. An unavailable endpoint never authorizes
killing
a numeric PID: a timed-out stop leaves cleanup ownership intact and directs you
to
the selected runtime's logs. Remote endpoint loss alone cannot prove cleanup;
remote completion remains unconfirmed until a completion protocol is available.

```bash
# Start or inspect the server
bmux server start
bmux server status
bmux server stop

# Create and attach sessions
bmux new-session dev
bmux list-sessions
bmux attach dev

# Work with named workspaces and tabs
bmux workspace new --name project
bmux tab new --name editor
bmux tab list
bmux workspace switch project

# Multi-client collaboration
bmux list-clients
bmux follow <client-uuid>                     # your attached terminal follows
bmux follow <client-uuid> --client <your-id>  # with several attachments, name one
bmux unfollow

# Remote access: pair a device once (host side)
bmux remote enable
bmux remote pair                 # prints a one-time link and waits to approve
bmux remote pair --role view-only --name laptop
bmux remote list
bmux remote status
bmux remote role laptop view-only
bmux remote revoke laptop
bmux remote disable

# Remote access: the connecting device (needs no server)
bmux connect --pair              # paste the link, compare the phrase
bmux connect desk                # attach to the saved host
bmux connect desk app            # attach to session `app`
bmux attach --target desk app
bmux --target desk list-sessions
bmux remote hosts
bmux remote rename desk desktop
bmux remote forget desktop

# Optional runtime instance selection (for parallel local runtimes)
bmux --runtime dev server start --daemon

# Native per-user login autostart (launchd, systemd user service, or Task Scheduler)
bmux server autostart install
bmux server autostart status
bmux server autostart print      # side-effect-free declaration output
bmux server autostart uninstall
# Named runtimes receive isolated native service identities:
bmux --runtime dev server autostart install
# Test the invoked binary with normal settings but fresh writable BMUX state.
# No arguments after -- are needed for interactive startup.
bmux sandbox dev --inherit-config
# Reconnect or explicitly stop the retained test server using its printed id/path:
bmux sandbox attach <id-or-path>
bmux sandbox stop <id-or-path>
# Ephemeral sandbox run (fully isolated config/runtime/data/state/logs)
bmux sandbox run -- server status
bmux sandbox run --bmux-bin ./target/debug/bmux --env-mode inherit -- --version
bmux sandbox dev -- server status
bmux sandbox list --limit 10
bmux sandbox status --json
bmux sandbox list --source playbook --limit 10
bmux sandbox inspect --latest
bmux sandbox inspect --latest --source recording-verify
bmux sandbox inspect --latest-failed --tail 120
bmux sandbox tail --latest-failed --tail 120 --json
bmux sandbox open --latest-failed --json
bmux sandbox rerun --latest-failed --bmux-bin ./target/debug/bmux --json
bmux sandbox triage --json
bmux sandbox triage --latest-failed --bundle --bundle-output ./sandbox-artifacts --json
bmux sandbox triage --latest-failed --bundle --bundle-strict-verify --json
bmux sandbox triage --latest-failed --rerun --bmux-bin ./target/debug/bmux
bmux sandbox bundle bmux-sbx-123 --output ./sandbox-artifacts
bmux sandbox bundle bmux-sbx-123 --include-env --verify --json
bmux sandbox verify-bundle ./sandbox-artifacts/bmux-sbx-123-1700000000000 --json
bmux sandbox verify-bundle ./sandbox-artifacts/bmux-sbx-123-1700000000000 --strict --json
bmux sandbox doctor --json
bmux sandbox doctor --fix --dry-run --json
bmux sandbox cleanup --dry-run --json
bmux sandbox clean --dry-run --json
bmux sandbox cleanup --all-status --source playbook --older-than 0
bmux sandbox cleanup --source recording-verify --older-than 600

# Logging
bmux logs path
bmux logs level
bmux logs tail
bmux logs path --json
bmux logs level --json
bmux logs tail --since 15m --lines 200
bmux logs watch --exclude "bmux server listening"
bmux logs watch --profile incident-db
bmux logs profiles list
```

Remote access is off until `bmux remote enable`, and only paired devices can
connect. Its listener, relay and auto-start settings live under
`[plugins.settings."bmux.connections".remote]`; see
[Remote access and pairing](remote-pairing.md#settings).

`bmux logs watch` uses a ratatui interface and supports Vim-style navigation
(`j`/`k`, `g`/`G`, `Ctrl-u`/`Ctrl-d`).

Top-level and grouped command forms are supported in many areas of the CLI.

All list commands with `--json` output a bare JSON array.

Logging defaults:

- file sink is enabled by default
- default level is `info`
- `--verbose` raises level to `debug`
- `--log-level` supports `error|warn|info|debug|trace`

Environment overrides:

- `BMUX_LOG_LEVEL`: effective runtime log level

A native autostart service runs foreground `bmux server start` so the operating
system owns supervision; it does not use `--daemon`. Installation is explicit
and starts immediately unless `--no-start` is passed. If remote access is
enabled (`bmux remote enable`), it also starts with the server at login.

Imperative installation refuses symlinked, read-only, conflicting, or externally
managed declarations. Nix/Home Manager users should define
`${pkgs.bmux}/bin/bmux server start` through `systemd.user.services` or
`launchd.agents` and let Nix own the declaration; bmux never mutates
`bmux.toml`. Linux autostart begins at login and does not enable lingering
automatically.

Runtime selection vs sandbox isolation:

- Use `--runtime <name>` to run multiple local bmux runtime instances side-by-side while still using your normal config/data/state roots.
- Use `bmux sandbox dev --inherit-config` for local build testing with a private snapshot of your normal settings and fresh BMUX config/runtime/data/state/log directories. It uses the invoked binary (or explicit `--bmux-bin`) and preserves your shell environment and working directory. Detaching retains the server; use the printed `sandbox attach` or `sandbox stop` command. A changed binary requires stopping the old sandbox before testing the rebuild. This is not an OS security sandbox: configured external paths, plugin artifacts, and external services remain shared. Do not use config snapshots as shareable diagnostics; they can contain secrets.
- Use `bmux sandbox ...` when you want a throwaway isolated environment (config/runtime/data/state/logs/home/tmp) for safe local build testing and failure triage.
- `BMUX_LOG_DIR`: explicit log directory
- `BMUX_STATE_DIR`: explicit state directory
- `BMUX_TARGET`: default command target (same behavior as `--target`)

`--target` and `BMUX_TARGET` name a saved host (see `bmux remote hosts`) or
`local`. Saved hosts come from pairing, not from `bmux.toml`.

Plugin command ownership policy is optional and declarative:

```toml
[plugins.routing]
conflict_mode = "fail_startup"

[[plugins.routing.required_namespaces]]
namespace = "plugin"

[[plugins.routing.required_paths]]
path = ["recording", "start"]
```

Role policy: `owner` controls session and window mutations plus role changes,
`writer` can send attach input, and `observer` is read-only.

Window presentation settings and the tab-bar/sidebar enablement combinations
are documented in [Window presentation plugins](tab-presentations.md).

## Examples

- Prompt showcase (isolated in-process sandbox + attach prompt API):

  ```bash
  cargo run -p bmux_prompt_showcase
  ```

- Plugin-provided prompt showcase (reuses `example.native` prompt sequence):

  ```bash
  cargo run -p bmux_prompt_plugin_showcase
  ```

- Native plugin example (the crate is an rlib; the shared packager builds it
  as the loadable artifact, see [examples/native-plugin](../examples/native-plugin/README.md)):

  ```bash
  python3 scripts/package-plugin.py \
    --implementation-manifest examples/native-plugin/Cargo.toml \
    --output target/plugin-packaging/native/backend --component backend --build
  ```

- Minimal hello plugin example:

  ```bash
  cargo build -p bmux_example_hello_plugin
  ```

See the [root README](../README.md) for installation and [TESTING.md](../TESTING.md) for validation. Examples using remote access, service installation, or plugin installation change runtime state; review them before running.
