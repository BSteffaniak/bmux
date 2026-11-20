# CLI bootstrap boundary

Baseline: `3320b28b58b6`. A namespace is not an owner. In particular the ability
to repair an installation must not depend on activating its recording, snapshot,
sessions, or connections backend.

| Retained entry points                                                                                        | Baseline path and reason                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| ------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `config path/show/get/set`                                                                                   | `runtime/config_cli.rs`: report candidates and existence; serialize effective `BmuxConfig`; traverse a dotted key; edit the selected document with `toml_edit`. These select/inspect the installation, including unknown plugin settings. Do not substitute a plugin's defaults for effective config or make reads write it.                                                                                                                                                                                                                                                                                                           |
| `config profiles list/show/resolve/explain/switch/diff/lint/evaluate`, `config scope explain`                | Same module, `bmux_config` composition and scoped loader. Switch includes preview versus persistent edit. Resolution includes environment, inherited layers and scoped cwd. The resolver is needed to select plugins; plugin discovery is not a substitute. Keymap diagnostics sharing this file do not make them config leaves.                                                                                                                                                                                                                                                                                                       |
| `server start/stop/status/whoami-principal`                                                                  | `dispatch_server_start` → `runtime/bootstrap.rs` (separate poll frame); `runtime/server_commands.rs` for probes/shutdown. These select an endpoint, own launch/readiness, request graceful stop, or report observed identity. Whoami's raw connection asks the server; equality of principal IDs is reported, not a client-issued authorization. Status preserves the baseline snapshot projection and latest event when running, JSON incl. nulls and nonzero exit when stopped. Save/restore are snapshot operations, not lifecycle; gateway hosting and kiosk/token decisions belong to remote access, not the local startup owner. |
| `server autostart install/uninstall/status/print`                                                            | `server_autostart.rs` checks local target and constructs per-user service specs. A service manager must be able to start the selected image without first reaching a plugin on a server. Printing/inspecting is not installing.                                                                                                                                                                                                                                                                                                                                                                                                        |
| `sandbox run/dev/attach/stop`                                                                                | `sandbox_cli.rs`, `sandbox_cli/inherited.rs`: start child in owned paths, track it and clean up according to keep/exit/timeout; inherited mode snapshots config and read-only plugin search roots, disables inherited gateway exposure and rejects conflicting child runtime/target/config. Attach/stop address the chosen sandbox, not an arbitrary named session.                                                                                                                                                                                                                                                                    |
| `sandbox list/status/inspect/tail/open/rerun/triage/doctor/bundle/verify-bundle/cleanup/clean/rebuild-index` | Same module: inventory, diagnostics, artifact verification and recovery of isolated children. `playbook`/`recording-verify` source labels and legacy `bpb-`/`brv-` directory classification are on-disk discovery compatibility, not execution of those workflows. Preserve latest/failed/source selection and destructive-operation safeguards.                                                                                                                                                                                                                                                                                       |
| `slot` and `env`: list/show/paths/doctor/install/uninstall/shell/exec/print                                  | Both dispatch the same `SlotCommand` to `slot_cli.rs`, using `bmux_slots` and `bmux_env`, also used by the standalone `bmux-env` image. Exec forwards trailing argv; shell/print render output without executing it. `runtime/slot.rs` resolves `BMUX_SLOT_NAME` before executable basename; `cli_parse.rs` propagates selected slot to children and selects base/slot config *before* discovery. Never treat an unknown slot as the default.                                                                                                                                                                                          |

## Process contracts, including non-help callers

- Daemon re-exec uses `server_start_child_arguments`: selected runtime/config and `server start --foreground-internal` plus pane integration/rolling overrides. `BMUX_BOOTSTRAP_RECORDING_START` carries the optional manual start request. Bterm's `runtime_launch.rs`, service startup and local integration fixtures also depend on the foreground entry. Keep launch fencing, argument order, selected executable and wait behavior; do not start a second server to implement a command.
- `server bridge --stdio` is a machine protocol used by the SSH connection transport (`plugins/connections-plugin/src/transport.rs`). `remote_cli.rs` checks `BMUX_BRIDGE_READY` from `--preflight`; stdout belongs to the transport. Neither probe authorizes a principal. A migration of gateway UX must preserve this remote executable contract independently of a local plugin registry.
- The broadcast plugin's `terminal.rs` starts `attach <target> --observer-socket …` (observer dimensions are hidden flags). Do not redirect this noninteractive subprocess to interactive attach UI. Kernel CLI execution in `runtime/plugin_kernel.rs` constructs `--core-builtins-only`; this bypass is not an implementation strategy for a removed domain leaf.
- `plugins/pane-runtime-plugin/src/runtime.rs::configure_device_seal_broker_env` supplies `SSHENV_DEVICE_SEAL_COMMAND=<current exe> device-seal-broker` under client scope. This internal entry is **not** marked `hide` at the baseline. `runtime/device_seal_broker.rs` reads opaque stdin, connects under normal connection policy, writes only returned bytes to stdout. `packages/server/src/lib.rs` selects an attached streaming event-push client, associates the pending request with that client and rejects replies from a different one; it waits up to 30 seconds. `runtime/attach/runtime.rs` matches target client before invoking the device-seal implementation and replying. Keep private operations on the selected client, not the pane, relay, CLI parser, or a new host API. Do not log requests/responses or use real credentials to test plumbing.

## Recording policy removed from startup

`packages/recording-bootstrap` owns the pre-activation decision to auto-start
rolling capture and constructs `RecordingPluginConfig` for both server startup
and standalone command execution. An explicit window of zero is invalid even
with recording forced off. An implicit configured zero does not start capture; a
positive window or explicit event/capture selection can request capture even
when config is off. Standalone commands get the selected defaults but never
implicitly auto-start recording or inherit a manual start request. The library
has no capture backend, transport, host registry, or subprocess authority. The
server still owns its process and the recording plugin still owns activation and
capture. CLI schema/defaults, child arguments, registration and activation order
are unchanged.

## Acceptance and limits

Run `python3 scripts/cli-bootstrap-boundary/contracts.py` to compare retained
enum definitions (including Clap annotations) with `git show
3320b28b58b6:packages/cli-schema/src/lib.rs`. Run `python3
scripts/cli-bootstrap-boundary/accept.py /absolute/path/to/bundled/bmux
/absolute/path/to/minimal/bmux` after building. Each invocation uses a new
private home, runtime, temp and config, no inherited auth environment, and both
default and explicitly disabled domain sets. It checks config inspection/edit,
slot/env equivalence, sandbox inventory, offline server status, bridge
preflight, hidden flag visibility and parser conflicts. It never installs a
service, connects to SSH, or invokes a credential operation. Run the recording
bootstrap unit tests with the workspace suite.

**Limits:** retained static ownership is not a guarantee that the baseline
parser works with a malformed config or a failing registry scan:
`parse_runtime_cli` loads both before dispatch, even for `--core-builtins-only`.
Recover with a known-good isolated installation/config; this lane does not add
an error-swallowing fallback. Non-bare default attach's session/context
selection in `bootstrap.rs`, snapshot RPCs during live status, server-side
recording settings normalization and broker target selection still depend on
their existing implementations. The noninteractive CLI checks do not establish
successful broker round trips, live snapshot status, or native service
installation. These need integration coverage by their respective owners.

### No-backend Unix terminal

With zero activated backends, the local server owns one lazy `bmux_pty`
execution. The local default CLI attachment uses a versioned byte stream over a
separate strict same-user IPC socket in that server's private runtime directory.
SSH, gateway and named targets do not select this transport. The shell program
comes from server configuration/environment, not the attachment request. A
client can send input, resize, or release the terminal with Ctrl-];
disconnecting does not drop the server's PTY. There are no named sessions,
workspace operations, or additional `HostRuntimeApi` methods. Detached output is
backpressured, not a persistent scrollback database.

`scripts/cli-bootstrap-boundary/build-and-test-pty.sh` builds the no-feature CLI
and the local-splits fixture. The bare case opens fresh controlling terminals,
checks markers from the same shell before and after Ctrl-] and client
termination, compares `stty -g` before and after each attachment, and checks
that shell exit 7 reaches the CLI. This exercises Unix termios and cooperative
client shutdown, not abrupt server death or Windows ConPTY.

### Default attach selection

Selection is pinned before bootstrap: only a local target can launch a local
server or open its same-user terminal socket. The connected server advertises
`core.local_terminal.v1` only after binding that socket. Unsupported clients and
servers do not negotiate it. Client plugin lists and failed service calls are
not fallback signals. An empty server or one whose active plugins are all
packaged and explicitly declare `core.local_terminal.compatible.v1` may offer
it; unknown, external, partial execution, and policy stacks fail closed. The
plugin event bridge continues running alongside the neutral listener. The PTY
fixture also builds a performance-only image, invokes its settings service, then
checks a persistent shell without `--target` or `--runtime`. A configured
unreachable TLS loopback target must report that target without spawning a local
shell.
