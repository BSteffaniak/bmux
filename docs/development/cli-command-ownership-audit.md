# CLI command ownership audit

Baseline: `3320b28b58b6`. This is a static inventory, not a list of commands
active in every profile. A manifest declaration, an exact-path ownership claim,
the Clap parser, and the handler that actually performs the work are four
different things. A plugin can supply the first two and still call back into the
built-in handler. An empty manifest match below is not proof that the operation
belongs in core.

## Where to find the implemented ownership map

[`command-dispositions.json`](../../scripts/cli-ownership/command-dispositions.json) records the disposition of each audited command or namespace: 70 migrated, 190 retained in plugins, 78 retained in core, 29 composed namespaces, and 15 non-CLI entries (hidden commands, including the sidebar and tab-bar show/hide/toggle namespaces). Use that catalog for current ownership; the family decisions below describe the starting inventory. [`scripts/cli-ownership`](../../scripts/cli-ownership/README.md) contains the frozen reference cases and four-profile comparison. An intentional help or availability change is not a byte-identical match to the historical command.

For the build-time dependency boundary, run `scripts/check-cli-qr-features.sh`
and `scripts/check-cli-iroh-features.sh`. For actual terminal behavior, use the
isolated PTY checks in `scripts/cli-bootstrap-boundary`; package reachability
alone does not prove that a shell starts or that a split receives input.

Owner-level acceptance harnesses run a built binary in private roots; none of
them runs under `cargo test`:

- `python3 plugins/plugin-cli-plugin/tests/admin_recovery.py /path/to/bmux`:
  plugin administration and repair with `bundled-plugin-cli`.
- `python3 packages/cli/tests/recording_snapshot_roundtrip.py --binary /path/to/bmux`:
  Unix PTY attach, recording and snapshot save/restore round trip.
- `python3 plugins/connections-plugin/src/hosted/acceptance.py /path/to/bmux`:
  hosted share/join/setup flows against a local fixture control plane (needs an
  adjacent pane supervisor and `zbarimg` or macOS Vision for QR decoding).

## Migration decisions

| Command families                                                                                                                      | Where the behavior belongs                                                                                                                                       | Boundary to preserve                                                                                                                                                                                                                                                                                                                 |
| ------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `share`, `unshare`, hosted `setup`, `host`, `hosts`, `join`, `auth`, `connect`                                                        | Connections. Migrate sharing first; separate local client UX from host administration.                                                                           | Today `run_share` also implements `share revoke`, resolves default targets, reserves a name with the control plane, persists the mapping, handles clipboard failure as a warning and renders a QR. Preserve all of these and the `control` role default. Never send interactive client output or access tokens to the wrong process. |
| `remote` (including trust, completions and deployment), `access`                                                                      | Connections owns target and access policy; installation and transport mechanisms may be shared.                                                                  | Pairing and SSH administration are not interchangeable. Preserve noninteractive completions, repeated key arguments, explicit target selection and authentication before changing handlers.                                                                                                                                          |
| `kiosk`, `server gateway`, `server bridge`                                                                                            | Separate remote access/token decisions from generic server process and byte transport.                                                                           | Do not infer permissions from connectivity or transfer token issuance to an unauthenticated client command. Trace launchers before moving hidden or subprocess entry points.                                                                                                                                                         |
| Session create/list/kill, client listing, follow/unfollow, named attach/detach, including top-level spellings                         | Sessions/clients own the named commands and policy. Retain only neutral attach, detach and terminal execution mechanisms in core.                                | Bare single-terminal attachment must work without either domain plugin. Do not implement fallback session management twice. Preserve explicit kill versus detach.                                                                                                                                                                    |
| `playbook`, recording extensions, `server save/restore`                                                                               | Separate playbook UX from recording/replay and snapshot persistence. Some recording commands already extend `playbook` and `server`.                             | Move leaf paths, not entire namespaces. The plugin-cli proxy is not the playbook engine. Verify offline validation and sandbox lifecycle separately from server RPCs.                                                                                                                                                                |
| `perf`, `logs`, `diagnostics`, `keymap`, `terminal`, `doctor`                                                                         | Performance policy and optional diagnostics UX can move; tracing, terminal capability probes, input mapping and bootstrap diagnostics remain usable by the host. | Do not move generic mechanism just because a plugin advertises the UX. For each proxy distinguish actual reusable mechanism from an entire domain workflow hidden behind CLI argv.                                                                                                                                                   |
| `server` lifecycle/status/whoami/autostart, `config` including profiles/scope, `sandbox`, `slot`/`env`, internal `device-seal-broker` | Keep neutral startup, configuration selection, isolation and process entry points. Extract domain-specific portions, not these families wholesale.               | These can run before plugin discovery or select its environment. Moving them behind normal plugin activation could make a broken installation impossible to diagnose or repair. The credential broker needs a subprocess caller/authority audit, not deletion based on lack of help visibility.                                      |
| Plugin administration and remaining plugin-advertised commands                                                                        | Leave working plugin implementations in place. Keep generic discovery, command parsing, collision detection, global flags and dispatch in core.                  | `plugin-cli-plugin/src/lib.rs`, `connections-plugin/src/client_cli.rs` and cluster membership/pane code use the core CLI bridge. Do not count a manifest or proxy alone as completed migration; do not introduce domain host APIs.                                                                                                   |

## Cutover and regression checks

For each family freeze old help, aliases, defaults, repeatable options,
positional ordering, validation errors, exit status, stdout/stderr, and
interactive vs noninteractive behavior. Compare bundled, explicitly disabled,
and plugin-absent builds, including compatibility clients. Where manifest
argument semantics differ from static Clap, extend the generic descriptor only
if needed. Remove the redundant static enum variants, execution table entries,
dispatch branches and their imports together, after tracing internal callers.
Test the plugin actually performs the side effect rather than forwarding back to
its old path. Keep a neutral fallback only where baseline operation requires
one.

For sharing specifically: use an isolated configuration and local control-plane
fixture; exercise `share --qr`, copy failure, unnamed/default target,
role/TTL/one-time, `unshare`, and `share revoke`. Decode the rendered QR and
compare the URL, including the `bmux://` fallback. Require `qrcode` absent from
the locked bare and local-splits graphs and present with connections. Repeat the
graph check for each claimed dependency saving; moving code alone is not
evidence.

## Built-in execution table

A row may denote a namespace, not a leaf. The last column matches **advertised
paths or aliases** in checked-in plugin manifests, independent of enablement; it
is not a claim about execution authority.

| Registered path              | Built-in handler ID        | Matching plugin declaration |
| ---------------------------- | -------------------------- | --------------------------- |
| `setup`                      | `Setup`                    | —                           |
| `host`                       | `Host`                     | —                           |
| `join`                       | `Join`                     | —                           |
| `hosts`                      | `Hosts`                    | —                           |
| `auth`                       | `Auth`                     | —                           |
| `auth login`                 | `AuthLogin`                | —                           |
| `auth status`                | `AuthStatus`               | —                           |
| `auth logout`                | `AuthLogout`               | —                           |
| `access`                     | `Access`                   | —                           |
| `kiosk`                      | `Kiosk`                    | —                           |
| `kiosk status`               | `KioskStatus`              | —                           |
| `kiosk init`                 | `KioskInit`                | —                           |
| `kiosk issue-token`          | `KioskIssueToken`          | —                           |
| `kiosk revoke-token`         | `KioskRevokeToken`         | —                           |
| `kiosk attach`               | `KioskAttach`              | —                           |
| `kiosk ssh-print-config`     | `KioskSshPrintConfig`      | —                           |
| `share`                      | `Share`                    | —                           |
| `unshare`                    | `Unshare`                  | —                           |
| `connect`                    | `Connect`                  | bmux.connections            |
| `new-session`                | `NewSession`               | bmux.sessions               |
| `list-sessions`              | `ListSessions`             | —                           |
| `list-clients`               | `ListClients`              | —                           |
| `kill-session`               | `KillSession`              | —                           |
| `kill-all-sessions`          | `KillAllSessions`          | —                           |
| `attach`                     | `Attach`                   | —                           |
| `detach`                     | `Detach`                   | —                           |
| `follow`                     | `Follow`                   | —                           |
| `unfollow`                   | `Unfollow`                 | —                           |
| `session`                    | `Session`                  | —                           |
| `session new`                | `SessionNew`               | —                           |
| `session list`               | `SessionList`              | —                           |
| `session clients`            | `SessionClients`           | —                           |
| `session kill`               | `SessionKill`              | —                           |
| `session kill-all`           | `SessionKillAll`           | —                           |
| `session attach`             | `SessionAttach`            | —                           |
| `session detach`             | `SessionDetach`            | —                           |
| `session follow`             | `SessionFollow`            | —                           |
| `session unfollow`           | `SessionUnfollow`          | —                           |
| `remote`                     | `Remote`                   | —                           |
| `remote list`                | `RemoteList`               | bmux.connections            |
| `remote test`                | `RemoteTest`               | —                           |
| `remote doctor`              | `RemoteDoctor`             | —                           |
| `remote init`                | `RemoteInit`               | —                           |
| `remote install-server`      | `RemoteInstallServer`      | —                           |
| `remote upgrade`             | `RemoteUpgrade`            | —                           |
| `remote trust`               | `RemoteTrust`              | —                           |
| `remote trust list`          | `RemoteTrustList`          | —                           |
| `remote trust add`           | `RemoteTrustAdd`           | —                           |
| `remote trust remove`        | `RemoteTrustRemove`        | —                           |
| `remote complete`            | `RemoteComplete`           | —                           |
| `remote complete targets`    | `RemoteCompleteTargets`    | —                           |
| `remote complete sessions`   | `RemoteCompleteSessions`   | —                           |
| `server`                     | `Server`                   | —                           |
| `server start`               | `ServerStart`              | —                           |
| `server status`              | `ServerStatus`             | —                           |
| `server whoami-principal`    | `ServerWhoamiPrincipal`    | —                           |
| `server save`                | `ServerSave`               | —                           |
| `server restore`             | `ServerRestore`            | —                           |
| `server stop`                | `ServerStop`               | —                           |
| `server autostart install`   | `ServerAutostartInstall`   | —                           |
| `server autostart uninstall` | `ServerAutostartUninstall` | —                           |
| `server autostart status`    | `ServerAutostartStatus`    | —                           |
| `server autostart print`     | `ServerAutostartPrint`     | —                           |
| `server gateway`             | `ServerGateway`            | —                           |
| `server bridge`              | `ServerBridge`             | —                           |
| `logs`                       | `Logs`                     | —                           |
| `logs path`                  | `LogsPath`                 | bmux.plugin_cli             |
| `logs level`                 | `LogsLevel`                | bmux.plugin_cli             |
| `logs tail`                  | `LogsTail`                 | bmux.plugin_cli             |
| `logs watch`                 | `LogsWatch`                | bmux.plugin_cli             |
| `diagnostics`                | `Diagnostics`              | —                           |
| `diagnostics watch`          | `DiagnosticsWatch`         | —                           |
| `logs profiles`              | `LogsProfiles`             | —                           |
| `logs profiles list`         | `LogsProfilesList`         | bmux.plugin_cli             |
| `logs profiles show`         | `LogsProfilesShow`         | bmux.plugin_cli             |
| `logs profiles delete`       | `LogsProfilesDelete`       | bmux.plugin_cli             |
| `logs profiles rename`       | `LogsProfilesRename`       | bmux.plugin_cli             |
| `config`                     | `Config`                   | —                           |
| `config path`                | `ConfigPath`               | —                           |
| `config show`                | `ConfigShow`               | —                           |
| `config get`                 | `ConfigGet`                | —                           |
| `config set`                 | `ConfigSet`                | —                           |
| `config profiles list`       | `ConfigProfilesList`       | —                           |
| `config profiles show`       | `ConfigProfilesShow`       | —                           |
| `config profiles resolve`    | `ConfigProfilesResolve`    | —                           |
| `config profiles explain`    | `ConfigProfilesExplain`    | —                           |
| `config profiles switch`     | `ConfigProfilesSwitch`     | —                           |
| `config profiles diff`       | `ConfigProfilesDiff`       | —                           |
| `config profiles lint`       | `ConfigProfilesLint`       | —                           |
| `config profiles evaluate`   | `ConfigProfilesEvaluate`   | —                           |
| `config scope explain`       | `ConfigScopeExplain`       | —                           |
| `perf`                       | `Perf`                     | —                           |
| `perf status`                | `PerfStatus`               | —                           |
| `perf on`                    | `PerfOn`                   | —                           |
| `perf off`                   | `PerfOff`                  | —                           |
| `doctor`                     | `Doctor`                   | —                           |
| `keymap`                     | `Keymap`                   | —                           |
| `keymap doctor`              | `KeymapDoctor`             | bmux.plugin_cli             |
| `keymap explain`             | `KeymapExplain`            | —                           |
| `terminal`                   | `Terminal`                 | —                           |
| `terminal doctor`            | `TerminalDoctor`           | bmux.plugin_cli             |
| `terminal install-terminfo`  | `TerminalInstallTerminfo`  | bmux.plugin_cli             |
| `playbook`                   | `Playbook`                 | —                           |
| `playbook run`               | `PlaybookRun`              | bmux.plugin_cli             |
| `playbook validate`          | `PlaybookValidate`         | bmux.plugin_cli             |
| `playbook interactive`       | `PlaybookInteractive`      | bmux.plugin_cli             |
| `playbook dry-run`           | `PlaybookDryRun`           | bmux.plugin_cli             |
| `playbook diff`              | `PlaybookDiff`             | bmux.plugin_cli             |
| `playbook cleanup`           | `PlaybookCleanup`          | bmux.plugin_cli             |
| `sandbox`                    | `Sandbox`                  | —                           |
| `sandbox run`                | `SandboxRun`               | —                           |
| `sandbox dev`                | `SandboxDev`               | —                           |
| `sandbox attach`             | `SandboxAttach`            | —                           |
| `sandbox stop`               | `SandboxStop`              | —                           |
| `sandbox list`               | `SandboxList`              | —                           |
| `sandbox status`             | `SandboxStatus`            | —                           |
| `sandbox inspect`            | `SandboxInspect`           | —                           |
| `sandbox tail`               | `SandboxTail`              | —                           |
| `sandbox open`               | `SandboxOpen`              | —                           |
| `sandbox rerun`              | `SandboxRerun`             | —                           |
| `sandbox triage`             | `SandboxTriage`            | —                           |
| `sandbox doctor`             | `SandboxDoctor`            | —                           |
| `sandbox bundle`             | `SandboxBundle`            | —                           |
| `sandbox verify-bundle`      | `SandboxVerifyBundle`      | —                           |
| `sandbox cleanup`            | `SandboxCleanup`           | —                           |
| `sandbox clean`              | `SandboxClean`             | —                           |
| `sandbox rebuild-index`      | `SandboxRebuildIndex`      | —                           |
| `slot list`                  | `SlotList`                 | —                           |
| `slot show`                  | `SlotShow`                 | —                           |
| `slot paths`                 | `SlotPaths`                | —                           |
| `slot doctor`                | `SlotDoctor`               | —                           |
| `slot install`               | `SlotInstall`              | —                           |
| `slot uninstall`             | `SlotUninstall`            | —                           |
| `slot shell`                 | `SlotShell`                | —                           |
| `slot exec`                  | `SlotExec`                 | —                           |
| `slot print`                 | `SlotPrint`                | —                           |
| `device-seal-broker`         | `DeviceSealBroker`         | —                           |

## Static schema cross-check

Rust variants, including nested and internal commands. This catches cases such
as `access` whose multiple operations share one execution-table entry.
`External` is the dispatch escape hatch; `Env` is a compatibility surface for
`Slot`.

- **Command**: Setup, Host, Join, Hosts, Auth, Access, Kiosk, Share, Unshare, Connect, NewSession, ListSessions, ListClients, KillSession, KillAllSessions, Attach, Detach, Follow, Unfollow, Session, Remote, Server, Logs, Diagnostics, Config, Perf, Doctor, Keymap, Terminal, Playbook, Sandbox, Slot, Env, DeviceSealBroker, External.
- **AuthCommand**: Login, Status, Logout.
- **SlotCommand**: List, Show, Paths, Doctor, Install, Uninstall, Shell, Exec, Print.
- **AccessCommand**: Status, Init, Add, List, Remove, Enable, Disable.
- **KioskCommand**: Status, Init, IssueToken, RevokeToken, Attach, SshPrintConfig.
- **RemoteCommand**: List, Test, Doctor, Init, InstallServer, Upgrade, Trust, Complete.
- **RemoteTrustCommand**: List, Add, Remove.
- **RemoteCompleteCommand**: Targets, Sessions.
- **PlaybookCommand**: Run, Validate, Interactive, DryRun, Diff, Cleanup.
- **SandboxCommand**: Dev, Attach, Stop, Run, List, Status, Inspect, Tail, Open, Rerun, Triage, Doctor, Bundle, VerifyBundle, Cleanup, Clean, RebuildIndex.
- **SessionCommand**: New, List, Clients, Kill, KillAll, Attach, Detach, Follow, Unfollow.
- **ServerCommand**: Start, Status, WhoamiPrincipal, Save, Restore, Stop, Autostart, Gateway, Bridge.
- **ServerAutostartCommand**: Install, Uninstall, Status, Print.
- **KeymapCommand**: Doctor, Explain.
- **DiagnosticsCommand**: Watch, View.
- **LogsCommand**: Path, Level, Tail, Watch, Profiles.
- **LogsProfilesCommand**: List, Show, Delete, Rename.
- **ConfigCommand**: Path, Show, Get, Set, Profiles, Scope.
- **ConfigScopeCommand**: Explain.
- **ConfigProfilesCommand**: List, Show, Resolve, Explain, Switch, Diff, Lint, Evaluate.
- **PerfCommand**: Status, On, Off.
- **TerminalCommand**: Doctor, InstallTerminfo.

## Advertised plugin surfaces

Canonical paths; the manifest remains authoritative for aliases and argument
definitions.

- **bmux.broadcast**: `broadcast`.
- **bmux.cluster**: `cluster-init`, `cluster-enrollment-token-create`, `cluster-enrollment-list`, `cluster-enrollment-revoke`, `cluster-credential-rotate`, `cluster-member-revoke`, `cluster-join`, `cluster-leave`, `cluster-members`, `cluster-up`, `cluster-status`, `cluster-gateway-reset`, `cluster-doctor`, `cluster-events`, `cluster-hosts`, `cluster-gateway-status`, `cluster-gateway-explain`, `cluster-gateway-doctor`, `cluster-gateway-history`, `cluster-gateway-history-export`, `cluster-gateway-history-clear`, `cluster-gateway-why`, `cluster-pane-new`, `cluster-pane-move`, `cluster-pane-retry`.
- **bmux.command_palette**: `command-palette show`.
- **bmux.connections**: `remote-enable`, `remote-disable`, `remote-pair-request`, `remote-approve`, `remote-pair`, `remote-list`, `remote-status`, `remote-revoke`, `connect`, `remote-rename`, `remote-forget`.
- **bmux.finder**: `finder show`.
- **bmux.pane_runtime**: `pane-padding show`, `pane-padding set`, `pane-padding configure`, `pane-padding reset`.
- **bmux.permissions**: `permissions`, `hot-path-policy`, `permissions-current`, `grant`, `revoke`, `grant-hot-path-override`, `revoke-hot-path-override`, `list-hot-path-overrides`.
- **bmux.plugin_cli**: `plugin list`, `plugin run`, `plugin rebuild`, `plugin doctor`, `logs path`, `logs level`, `logs tail`, `logs watch`, `logs profiles list`, `logs profiles show`, `logs profiles delete`, `logs profiles rename`, `keymap doctor`, `terminal doctor`, `terminal install-terminfo`, `playbook run`, `playbook validate`, `playbook interactive`, `playbook dry-run`, `playbook diff`, `playbook cleanup`.
- **bmux.presenter**: `present play`, `present host`, `present join`, `present check`.
- **bmux.prompted_actions**: `prompted-action run`.
- **bmux.recording**: `record`, `server recording`, `playbook from-recording`, `recording start`, `recording stop`, `recording status`, `recording path`, `recording list`, `recording delete`, `recording delete-all`, `recording cut`, `recording inspect`, `recording analyze`, `recording replay`, `recording verify-smoke`, `recording export`, `recording prune`, `recording process start`, `recording process run`, `recording process status`, `recording process read`, `recording process screen`, `recording process send`, `recording process wait`, `recording process terminate`, `recording process finish`.
- **bmux.sessions**: `new-session`.
- **bmux.tabs**: `explain-spawn-directory`, `new-tab`, `rename-tab`, `rename-tab-by-id`, `list-tabs`, `kill-tab`, `kill-all-tabs`, `switch-tab`, `move-tab`, `next-tab`, `prev-tab`, `last-tab`, `goto-tab`, `close-current-tab`, `reset-order`, `focus-pane-in-direction`, `split-pane`, `resize-pane`, `zoom-pane`, `list-floating-panes`, `create-floating-pane`, `focus-floating-pane`, `raise-floating-pane`, `lower-floating-pane`, `focus-next-floating-pane`, `raise-active-floating-pane`, `lower-active-floating-pane`, `close-active-floating-pane`, `move-floating-pane`, `resize-floating-pane`, `move-active-floating-pane`, `resize-active-floating-pane`, `set-floating-pane-z`, `set-floating-pane-layer`, `close-floating-pane`, `close-active-pane`, `restart-pane`.
- **bmux.theme**: `theme show`, `theme refresh`.
- **bmux.workspaces**: `workspace activate-tab`, `workspace new`, `workspace list`, `workspace switch`, `workspace rename`, `workspace kill`, `workspace move-tab`, `workspace next`, `workspace previous`, `workspace last`.
