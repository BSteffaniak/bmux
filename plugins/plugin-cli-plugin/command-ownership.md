# CLI owner handoff (baseline `3320b28b58b6`)

Counts below are `[[commands]]` entries with `expose_in_cli = true` in
`git show 3320b28b58b6:plugins/<owner>-plugin/plugin.toml`, not a claim that
a particular bundle enables every command. Aliases do not add commands.
The implementation anchors are relative to `plugins/<owner>-plugin/src/`.

| Owner            | Entries | Disposition and implementation anchor                                                                                                                                                                                                                                                                                                                                            |
| ---------------- | ------: | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| broadcast        |       1 | Keep. `lib.rs::run_command` handles broadcast, not core argv.                                                                                                                                                                                                                                                                                                                    |
| cluster          |      25 | Keep cluster UX and gateway dispatch (`lib.rs::run_command`, `gateway.rs`). Membership health and pane/workspace readiness depend on the two remote probes, below.                                                                                                                                                                                                               |
| command-palette  |       1 | Keep. `lib.rs::command_now` enters the callback binding and calls `show`.                                                                                                                                                                                                                                                                                                        |
| connections      |      11 | Keep the paired-device operations. `client_cli.rs::forward` is the exception: `connect` forwards unpaired or missing targets to built-in `connect`; a paired target calls built-in `attach` with a paired-terminal URI. Preserve local-only invocation and provider selection. Do not conflate the former proxy with the latter terminal mechanism. Hosted lane owns this split. |
| finder           |       1 | Keep. `lib.rs::command_now` opens the finder via `show`.                                                                                                                                                                                                                                                                                                                         |
| pane-runtime     |       4 | Keep. `lib.rs::run_command` dispatches padding commands into `commands::run` against the plugin host.                                                                                                                                                                                                                                                                            |
| permissions      |       8 | Keep. `lib.rs::run_command` calls `handle_command` for policy queries/grants. No substitution of transport success for policy approval.                                                                                                                                                                                                                                          |
| plugin-cli       |      21 | Keep list/run/rebuild/doctor in their respective modules. The other 17 baseline entries were compatibility proxies, not implementations. Remove only together with a working owner and corresponding core cutover.                                                                                                                                                               |
| presenter        |       4 | Keep. `lib.rs::run_command` parses `cli::Command` and executes the presentation workflow.                                                                                                                                                                                                                                                                                        |
| prompted-actions |       1 | Keep. `lib.rs::command_now` checks cancellation and enters the callback binding before `run`.                                                                                                                                                                                                                                                                                    |
| recording        |      26 | Keep recording leaves including mixed namespaces. `lib.rs::run_command` dispatches interactive `record`, `process_commands`, and the recording owner. These are not aliases for the plugin-cli playbook proxies.                                                                                                                                                                 |
| sessions         |       1 | Keep `new-session`; `lib.rs::run_command` invokes `new_session_with_context`. Session lane owns the other named-session spellings.                                                                                                                                                                                                                                               |
| tabs             |      37 | Keep. `lib.rs::run_command` and its command match handle tab and pane operations inside the plugin.                                                                                                                                                                                                                                                                              |
| theme            |       2 | Keep. `views.rs` routes `show-theme` and `refresh-theme` to theme operations.                                                                                                                                                                                                                                                                                                    |
| workspaces       |      10 | Keep. `lib.rs::run_command` handles workspace commands with `WorkspaceEvents`.                                                                                                                                                                                                                                                                                                   |

## Bridge contract

`git grep -n core_cli_command_run_path 3320b28b58b6 -- plugins` identifies
three production users: plugin-cli, connections, cluster. The remaining
matches are the cluster runtime trait/adapter and test implementations.
`plugin run` itself is **not** a run-path caller. It checks the selected
registry, handles help, rejects self-invocation, and uses `plugin_command_run`
with the original argument vector, printing any result diagnostic and returning
the command's status. Keep the host-selected invocation authority.

In cluster, `membership.rs::run_health_probe` serves CLI and service status,
pane readiness/retry and workspace placement. The baseline sends exactly
`[remote, test]` or `[remote, doctor]`, plus a single target argument, through
the caller's host context. Nonzero exit becomes `probe exited with status N`;
service denial/unavailability becomes `probe failed to run: ...`. A selected
core call without an admitted handler is not permission to use an ambient
runtime. `gateway.rs::run_plugin_command_on_target` separately invokes the
plugin command operation through the connections endpoint client; it is not
a replacement local health-probe transport.

The validated migration unit is a leaf, not a namespace: reconcile the old
manifest's `owns_paths`, command definition and explicit route with the new
owner. Do not delete the built-in remote probes until Connections implements
the same target selection, trust, output and exit behavior and the cluster
adapter can call it on the existing admitted context. No connection or
credential material is needed for the registry or route checks.

## Diagnostics dispositions

| Command                                              | Execution owner and evidence                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `diagnostics watch/view`, `logs watch/tail/profiles` | `bmux.plugin_cli` owns filtering, follow mode, selected-state profile persistence and presentation. The CLI's diagnostics tracing layer writes the event stream even without the viewer. `diagnostics_cli::tests::parses_logs_watch_flags` covers filter forwarding; `logs_tail::tests::line_matches_since_uses_rfc3339_prefix` covers persisted lines.                                                                                                                                                                                                                                                                      |
| `logs path`                                          | The enabled plugin resolves the same segmented or flat log file used by tail, without a core command bridge (`logs_path_runs_without_a_host_bridge`). The static fallback remains available to inspect local logging without the plugin. `ConfigPaths::logs_dir` is the common platform/environment directory resolver.                                                                                                                                                                                                                                                                                                      |
| `logs level`                                         | Report the CLI process's initialized tracing level (`init_logging` / `EFFECTIVE_LOG_LEVEL`), not what a plugin infers by rereading config. `run_selected_core_command` is the selected compatibility caller.                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `keymap doctor/explain`                              | Diagnose the host input compiler, including user overrides, modal bindings, and contributions from enabled plugins. Attach's `build_attach_help_lines` consumes `effective_attach_keybindings`; `keymap explain` consumes `filtered_attach_keybindings`. The plugin command context's active bindings are for one attached mode, not an authoritative standalone configuration snapshot. Do not approximate explain by searching that list. Tests: `selected_trace_and_keymap_use_supplied_roots_and_registry`, `effective_attach_keybindings_include_scope_and_canonical_action_names`, `parses_keymap_explain_subcommand`. |
| `terminal doctor/install-terminfo`, local `doctor`   | `doctor_cli::run_doctor_text` checks terminal availability and recommends the installation command; `scripts/compat-matrix.sh` consumes `terminal doctor --json --trace`. Keep the selected trace file, pane TERM fallback, confirmation and check-only exit code in the host. Local `--core-builtins-only doctor` must remain independent of activation. Tests: `pane_term_falls_back_to_xterm_then_screen`, `parses_terminal_doctor_trace_filters`, `parses_terminal_install_terminfo_flags`.                                                                                                                              |
| `perf on/off/status`                                 | `bmux.performance` owns the profile default, read-modify-write and output. It calls the selected settings service. Attach and remote capture still measure and emit events. `parses_perf_on_defaults_to_detailed_profile` and `selected_host_failure_is_not_default_settings` guard the division.                                                                                                                                                                                                                                                                                                                            |

The default-profile help comparison against `3320b28b58b6` returned status 0 on
all of these help probes. Replacing opaque `[ARGS]...` with typed log options is
intentional. Viewer help must describe filter case and repeatability,
saved-profile defaults, and time syntax. `perf on` must advertise its actual
default and choices. Remaining shared-parser differences are namespace summaries
and ordering, `<COMMAND>` versus `[COMMAND]`, the implicit `help` subcommand,
and alias display text. Leaf metadata does not control synthetic parent
commands. A successful `--help` comparison alone does not test the TUI, trace
contents, or repair side effects.

## Recovery before activation

`plugin list`, `plugin run`, `plugin rebuild`, and `plugin doctor` have no
built-in implementation. List filters the selected registry. Run delegates to
the selected command through the caller's service port. Rebuild discovers
workspace packages, resolves selectors and uses the packager; doctor inspects
declarations, capability requirements, command conflicts and entry readiness.
A disabled plugin does not get an alternate core implementation of these.

An invalid enabled dependency can prevent *all* plugin commands, including
`plugin doctor`, from activating. For this case, `--core-builtins-only doctor
--json` checks the enabled registry and its capability providers without
constructing command state. Core-only config path/get/set remain available to
locate and repair the configuration. This path still requires a parseable
config, respects slot selection, and does not retry any plugin call. It does
not load a plugin to determine whether that plugin can load.

Run `python3 plugins/plugin-cli-plugin/tests/admin_recovery.py /path/to/bmux`
against a build with `bundled-plugin-cli`. It exercises a non-executable
process entry and its repair, a missing dependency preventing activation,
self-invocation rejection, and disabling the administration plugin. The
fixture has private config, home, state and runtime roots and no ambient PATH.

Since the baseline audit, the Connections client no longer forwards unpaired
`connect` to core. Its remaining forward is to `attach` for a paired terminal.
Cluster still depends on the retained remote test/doctor operations; those
requests use the caller's existing authority, not a new host method.
