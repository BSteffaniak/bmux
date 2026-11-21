# Plan: replace the pane execution supervisor with in-server process management

Status: implemented on `bcode/prune-deps` (2026-11-21). The sections below
describe the design as built; "Deviations" lists where it differs from the
original proposal. User-facing behaviour is documented in
`docs/pane-execution.md`.

## Goal

Run each pane's shell directly under the bmux server. Today every pane needs two
extra supervisor processes, and bmux ships with a second binary.

What gets removed:

- **The second shipped binary.** `bmux-pane-execution-supervisor` goes away, so
  bmux ships as one binary.
- **The extra processes per pane.** Each Unix pane currently runs three
  processes: a guardian, an owner, and the shell. Two of those are the
  supervisor. That is about 4.7 MB of private memory per pane, or about 370 MB
  with 79 panes.
- **About 6k lines of code** in the supervisor binary and its client, plus their
  tests.

What we keep:

- **The same cleanup boundary.** All of a pane's processes are cleaned up,
  including jobs that ignore SIGHUP (`nohup`, disowned jobs), on:
  - pane close
  - natural shell exit
  - graceful server shutdown
  - server panic
  - fatal signals (SIGSEGV, SIGBUS, SIGABRT and similar)
- **Windows keeps the full guarantee**, including when the server is killed
  outright. The server holds a kill-on-close Job Object per pane.

What changes:

- **SIGKILL or OOM kill of the server on Unix.** The kernel still sends SIGHUP
  immediately, the same as tmux and zellij. Jobs that ignore SIGHUP are swept
  when the next server starts, not immediately.
- **Exit status after a server crash is lost.** Today it is already never read
  back after a restart.

## How it works today

These are the facts the plan depends on:

- **Process chain.** The server starts the guardian. The guardian calls
  `setsid`, becomes session leader, and holds the controlling tty without
  holding a slave descriptor. The guardian starts the owner, and the owner
  starts the target shell.
- **Lease.** A socketpair on fd 3 ties the guardian to the server. When it
  closes, the guardian force-stops the pane and sweeps its session.
- **Cleanup on natural exit.** When the target exits, the owner kills every
  other process group in the session. That includes HUP-ignoring and `nohup`
  jobs.
- **The supervisor is mandatory.** `select_backend` ignores `_required`.
  `readiness::check_admission` fails every spawn and every reservation when the
  helper is missing. The direct-PTY branches in `runtime.rs` are unreachable,
  and they never set `terminal_session_ended` or `terminal_owner_retired`.
  `docs/distribution.md` says ordinary shells fall back to a direct PTY; that is
  stale.
- **Recovery never reads supervisor state.** `reconcile` always returns
  `supervisor: None`. Nothing reads `lifecycle.json`, `receipt-*.json`, or the
  owner outcome files back.
- **State leaks.**
  - `supervisor-<token>.json` locators and `/tmp/bmux-exec-*` directories are
    never deleted.
  - `settled` is kept only in memory. After a restart, `supervisor::idle_blocker`
    and `readiness::blocker` therefore stay blocked.
  - `State/auxiliary-images` is never pruned.
  - `retire` has no caller, so any v4 reservation blocks managed idle forever.
- **`bmux_pty` already does the right spawn on Unix.** `Slave::spawn` calls
  `setsid` and `TIOCSCTTY` in `pre_exec` (`packages/pty/src/pair/unix.rs`).
- **Only the pane runtime uses auxiliary executables.** Its supervisor is the
  only declared auxiliary executable.

## Design

### Process model (Unix)

The server spawns the shell directly on the PTY through `bmux_pty`. The shell
becomes session leader, so its pid is the session id (sid) and also its process
group id (pgid).

The server keeps one `PaneSession` per pane:

- the leader pid, which is also the sid
- the leader's start identity, from `process_start_identity_for_pid`
- the PTY master

`target_pid` is the leader pid. The existing branch at `runtime.rs:8288` already
uses it.

### Session sweep (new module: `runtime/session_sweep.rs`)

This module is ported from the supervisor's `session_cleanup.rs`,
`linux_process_groups.rs` and `cleanup_probe.rs`.

**Discovery: find every live, non-zombie process whose sid equals the pane's
sid.**

- On Linux, scan `/proc` with a `getsid` pre-filter, then parse `stat`.
- On macOS and other Unix systems, run `ps -axo pid=,pgid=,stat=` from a fixed,
  trusted path, bounded by a deadline.
- Unlike today, the leader's own group is included.

**Kill.** The supervisor's join-group-then-kill trick does not work from outside
the session, because `setpgid` only works within the caller's own session.

- **Linux:** kill each process individually without a reuse race:
  1. Open a pidfd for the pid.
  2. Check that its sid matches and its start identity is as expected.
  3. Use `pidfd_send_signal(SIGKILL)`.
- **macOS:**
  1. Re-check `getsid` on a member of the group immediately before sending
     `kill(-pgid, SIGKILL)`.
  2. The remaining window is: the whole group dies and an unrelated process is
     given the same number as a new group in the gap. tmux has the same window.
     We accept it and document it.
- **Verify:** rescan until the session is empty, using the existing 3-pass,
  500 ms poll and 2 s deadline (`sweep_with`). Groups still present become
  `cleanup_error`.

**Scope.**

- **Processes that call `setsid` to escape stay out of scope**, as they are
  today. Keep the test
  `ended_terminal_preserves_escaped_descendant_without_late_signal_authority`.
- **The sweep never signals after the session has been verified empty.**
- **The sweep runs on a blocking thread**, never on the async runtime.

### Lifecycle mapping

| Event                               | In-server behavior                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| ----------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Natural shell exit                  | The waiter reaps the leader and runs `record_exit`. When `sweep_on_exit` is true (the default), the session sweep then runs, so background jobs die with the shell. Then `terminal_session_ended` and `terminal_owner_retired` are set. `cleanup_complete` becomes true once the sweep has verified the session is empty. When `sweep_on_exit` is false, surviving jobs keep running (tmux behaviour), and the pane's session record is dropped instead of swept. |
| Cooperative stop (`Term`)           | SIGTERM to the leader pid only, matching today.                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Force stop (`RequestStop`)          | A session sweep that includes the leader. `group_kill_issued` and `target_stop_issued` are set locally.                                                                                                                                                                                                                                                                                                                                                           |
| Pane close                          | Force stop, then close the master.                                                                                                                                                                                                                                                                                                                                                                                                                                |
| Graceful shutdown or panic (unwind) | Sweep every live `PaneSession` before exiting.                                                                                                                                                                                                                                                                                                                                                                                                                    |
| Fatal signal                        | Install a `sigaction` handler for SIGSEGV, SIGBUS, SIGILL, SIGFPE and SIGABRT. Also SIGABRT from `panic = "abort"` profiles such as `small`. The handler walks a fixed-capacity, lock-free table of `(sid, master_fd)` and, for each pane, sends `kill(-sid, SIGKILL)` and `kill(-tcgetpgrp(master), SIGKILL)`, both async-signal-safe. Then it restores the default handler and re-raises. Background groups are left to the startup sweep.                      |
| Server SIGKILL or OOM kill          | The kernel closes the masters and sends SIGHUP. Leftovers are swept by the next server start (see below).                                                                                                                                                                                                                                                                                                                                                         |

### Exit-sweep setting

This is a plugin-owned setting, parsed next to the existing `padding` settings
in
`plugins/pane-runtime-plugin/src/lib.rs`, and not part of core
`packages/config`:

```toml
[plugins.settings."bmux.pane_runtime".execution]
sweep_on_exit = true   # default; false leaves background/nohup jobs running after a normal shell exit
```

- **Unaffected by the setting.** Pane close, force stop, server shutdown and
  crash recovery always sweep. The setting only controls what happens after a
  natural exit.
- **When `false`, `cleanup_complete` means less.** It only means the leader has
  been reaped; it is no longer a guarantee that the session is empty. Document
  this for consumers of `cleanup_complete`.
- **On Windows, `false`** means the job handle is closed without
  kill-on-close. Clear `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` with
  `SetInformationJobObject` before dropping the handle.

### Crash records and startup recovery

**Record format.** Write `runtime_dir/pane-sessions/<execution-token>.json` with
`atomic_state::commit` once the shell is running. It holds the sid, the leader
start identity, and the tty name. Delete it when `cleanup_complete` is reached.
Pane-runtime activation already owns `runtime_dir`, so this belongs there and
not in core.

**On activation, before admitting new spawns:**

1. **Sweep each record that is still present.** Before sweeping, check that the
   session really is ours:
   - If a live process exists whose pid equals the sid, its start identity must
     match the record. If it does not, the number was reused by an unrelated
     session: skip it and drop the record.
   - If the leader is gone, the session may still be ours as long as members
     remain. Both kernels refuse to reuse a pid that is still in use as a
     session id. Phase 0 verifies this for XNU. Additionally require
     `member start time >= leader start time`.
2. **Reconcile v4 receipts.** Move each non-terminal `launch-*.json` (`Launching`,
   `Running` or `Uncertain`) to:
   - `Exited`, with `exit_code = None`
   - error `"server stopped before the execution ended; exit status lost"`
   - `cleanup_complete = true`, once its session was swept
   - capture left `Incomplete`
   - then **automatic retirement**, in the same recovery pass, after any input
     receipts still in `Writing`/`Uncertain` have been resolved to `Rejected`.
     Nothing needs an explicit `retire` call after a crash.
3. **Delete leftover legacy supervisor state:**
   - `supervisor-*.json` locators in `runtime_dir`
   - `/tmp/bmux-exec-*` directories that we own
   - this runtime's `State/auxiliary-images/<scope>/` subtree

   Old guardians are tied to their server's lifetime. A new server for the same
   `runtime_dir` means the old one and its guardians are gone, so deleting these
   is safe.

**Optional follow-up.** An attached client that sees the server socket close
could trigger the same sweep immediately. It reuses an existing process.

### Windows

**`bmux_pty` change.** `PtySlave::spawn` (`pair/windows.rs::start`) gains an
optional job. It uses `PROC_THREAD_ATTRIBUTE_JOB_LIST` next to the existing
`PSEUDOCONSOLE` attribute, so job assignment happens atomically at process
creation. `bmux_pty/src/conpty.rs:286-375` already does this for `PtyCommand`,
so that code is the template.

**Port from `windows_job.rs` into the pane runtime:**

- job creation with `KILL_ON_JOB_CLOSE`, without breakaway flags
- environment block validation and ordinal sort
- `command_line` and `quote`, including the `cmd /S /C` special case

**Spawn differences.** `bmux_pty` resolves `PATH`/`PATHEXT` itself and replaces
an invalid cwd. Managed launches must keep today's semantics, via a raw command
line plus `env_clear`.

**Execution handles.** Each execution holds `job: OwnedHandle` and the
`PtyChild`:

- **Exit code:** `PtyChild::wait` → `record_exit`.
- **After a natural exit:** `TerminateJobObject` kills any descendants.
- **Force stop:** `TerminateJobObject(job, 1)`.
- **`cleanup_complete`:** `ActiveProcesses == 0`, reported through a job
  completion port (`JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO`).
- **Cooperative stop:** stays unsupported and escalates to a force stop.

**Teardown.** Drop the job handle on plugin teardown. On a server crash the
kernel closes it and kill-on-close fires.

**Unchanged:** `ObservedSlave` is removed. `conpty_startup` (the early `CSI 6n`
reply) stays. `close_retired_console` now triggers on "job empty". Remove the
stale `taskkill /T` comment at `runtime.rs:14059`.

### API and semantics changes

- **`pane_runtime_execution_v4`:**
  - Remove `terminal-supervisor-locator` and `TerminalReconciliation.supervisor`;
    they are always `None` already.
  - The recording `process_store` persists this struct, so add `serde(default)`
    or migrate it.
  - `cleanup_complete`, `terminal_session_ended`, `exit_code` and the phases
    keep their names. Their meanings are as in the table above, plus the
    crash-recovery meaning.
- **Retirement after a crash:** recovery retires crashed reservations itself
  through an internal path. It accepts `Exited` with a crash-recovery error and
  capture `Incomplete`. The public `retire` keeps its current preconditions.
  Separately, give public `retire` a real caller, such as recording `finish`.
  Otherwise, reservations that ended normally still block managed idle.
- **Recording and connections consumers:** a retired, crash-recovered execution
  must read as terminal. Recording `refresh` reports it as finished with
  "exit status lost". Connections reopen forgets it and reserves a new
  execution.
- **`pane_runtime_readiness_v1`:**
  - `helper_ready` / `helper_error` become `execution_ready` /
    `execution_error`. Consumers to update: `packages/bterm/src/runtime_launch.rs`,
    `packages/bterm/src/headless.rs`,
    `packages/cli/src/runtime/managed_companion.rs`, and the bterm readiness
    integration test.
  - Drop the artifact, protocol and hash checks.
  - Keep the managed executable-identity check.
  - `restart_blocker` stops scanning `supervisor-*` and checks `pane-sessions/`
    and the receipts instead.
- **Application resurrection:** `runtime.rs:7847` currently disables it when
  there is no supervisor control. Make it always eligible.
  `shell_member` already accepts a shell that is its own session leader.
- **Connections:** `terminal_attach.rs` reopen should forget an execution that
  was swept after a crash and reserve a new one, rather than failing with
  "requires recovery" forever.

## Phases

Each phase builds and passes clippy and nextest on its own.

### Phase 0: verify assumptions

1. **PID reuse on XNU.** Confirm that XNU will not reuse a pid while it is still
   used as a pgid or sid. Check the `forkproc` pid search, and write a
   test-program check.
2. **Signal reset parity.** Compare `bmux_pty::prepare_child` with the owner's
   reset list. It needs to add PIPE, TTOU, TTIN and TSTP, and clear the signal
   mask.
3. **Raw environment.** Confirm that `SpawnCommand` can carry a raw byte
   environment (`native_env`) and can skip PATH resolution and `SHELL`/`arg0`
   injection for managed launches. Extend it if not.
4. **Shutdown and panic hooks.** Locate the server's existing shutdown and panic
   hooks, so the sweep runs in the right place.

### Phase 1: Unix in-server backend

1. **Session sweep.** Add `runtime/session_sweep.rs` with ported discovery, the
   Linux pidfd kill, the macOS checked `kill(-pgid)`, and verification. Port
   the unit tests from `session_cleanup.rs`, `linux_process_groups.rs` and
   `cleanup_probe.rs`.
2. **Direct-PTY path.** Complete the dormant direct-PTY path in `runtime.rs`:
   spawn (around 8259), pgid = sid, and waiter → `record_exit` → sweep → the
   `terminal_session_ended` and `terminal_owner_retired` fences.
3. **Stop.** `signal_owned_group` becomes cooperative = SIGTERM to the leader,
   force = session sweep.
4. **Exit-sweep setting.** Add the plugin setting
   `execution.sweep_on_exit` (default `true`) and its settings parsing tests.
5. **Switch backends.** `select_backend` returns `None` on Unix. The supervisor
   code stays compiled, but is unused on Unix.

### Phase 2: crash coverage

1. **Lifecycle hooks.** Add the sweep on graceful shutdown and on panic, and the
   fatal-signal handler with its lock-free `(sid, master_fd)` table.
2. **Records and recovery.** Add the `pane-sessions/` records, the activation
   sweep, receipt reconciliation with automatic retirement, and deletion of
   legacy state.
3. **Blockers and consumers.**
   - Rewrite `idle_blocker` and `readiness::blocker`.
   - Give public `retire` a caller.
   - Update recording `refresh` and connections reopen for retired,
     crash-recovered executions.

### Phase 3: Windows backend

1. **`bmux_pty`.** Add the job option to `bmux_pty` Windows `Slave::spawn`.
2. **Server-held job.** Server-held job per execution, the ported
   env/command-line code, and completion-port `cleanup_complete`.
3. **Switch backends.** `select_backend` returns `None` on Windows.
4. **Port Windows tests.** The `windows_job.rs` unit tests, plus the two ignored
   ConPTY integration tests, rewritten as "killed when the job handle drops".

### Phase 4: delete the supervisor

In the pane runtime:

- `src/bin/execution_supervisor.rs` and `src/bin/execution_supervisor/*`,
  including the two Python scripts. `supervisor_crash_tests.py` is already stale
  (it targets VERSION 6).
- `src/supervisor_protocol.rs`, `src/supervisor_receipts.rs`,
  `runtime/supervisor.rs`, `runtime/supervisor/*` and `runtime/supervisor_tests.rs`.
- `tests/execution_helper_artifact.rs`.
- The `Cargo.toml` `[[bin]]`, the `plugin.toml` `[[auxiliary_executables]]`
  entry, and `EXECUTION_SUPERVISOR_PROTOCOL_VERSION` with its test.
- In `runtime.rs`, the supervisor branches: `GuardianStartupChild`,
  `SupervisionCompletion` and `LifecycleCompletion`, and about 15 tests that
  use `install_test_artifact` or `inject_missing_supervisor`.
- `ExecutionCapture.supervisor` and `lifecycle_interrupter`.

Elsewhere:

- The supervisor fixture in the connections `device_client.rs` tests.
- In `packages/cli/tests/`: the supervisor assertions in
  `interactive_recording_integration.rs`, and the three `#[ignore]` tamper tests
  in `recording_process_integration.rs`.
- The BPDL and readiness renames from "API and semantics changes", and the
  application-resurrection flip.
- Windows crate features that are now unused, such as `Pipes`, `IO` and
  `Security_Authorization`. Confirm with machete.

### Phase 5: packaging

Recommendation: **delete the generic auxiliary-executable mechanism.** Nothing
else uses it, and keeping one binary is the point of this change. It is in git
history if a plugin ever needs it.

Keep:

- `executable_sha256`, `retain_executable` and `try_clone_snapshot`, which
  `shell_callback.rs` and `scoped.rs` use.
- The `artifacts.json` `Inventory` type, with `unavailable_auxiliary_executables`
  still tolerated when reading old archives. `Inventory` uses
  `deny_unknown_fields`.
- Plugin ABI 1.1 and `NativeLifecycleContextV1`. Plugins pin 1.1, and the
  auxiliary maps are skipped when empty, so they are wire-compatible.

Remove:

- **Manifest:** the `auxiliary_executables` manifest field and its validation,
  `auxiliary.rs` resolve and `validate_inventory`, `retained.rs` if it is unused
  afterwards, and the loader's `resolve_lifecycle_auxiliaries`.
- **SDK:** the `NativeLifecycleContext` auxiliary maps and their struct-literal
  initializers in about 8 plugins, and `ResolvedAuxiliaryExecutable`.
- **xtask:**
  - Remove `declarations`, the helper loop in `build`, and the declaration
    parameters of `stage`, `bterm` and `bterm_run`.
  - Rename `auxiliary stage` and `auxiliary nfpm` to `stage` and `nfpm`.
  - Update `release.yml` and `release-npm.yml`.
  - Drop xtask's `bmux_plugin` dependency if machete agrees.
- **Scripts:** in `smoke-pty-runtime.sh`, `compat-matrix.sh`,
  `cli-bootstrap-boundary/build-and-test-pty.sh` and
  `clipboard/linux/product.sh`, drop the `-p bmux_pane_runtime_plugin --bins`
  build and the adjacency checks, and update `test-live-deployment-boundary.py`.
- **Docs:** rewrite `docs/distribution.md` (auxiliary contract, stale fallback
  text), `docs/windows-platform-backends.md` (managed execution) and
  `docs/recording-process.md`. Fix `docs/bterm-reconciliation.md` and
  `docs/bterm-managed-storage.md`.

### Phase 6: validation

**New tests:**

- **Unix:**
  1. A `nohup sleep` job is killed on pane close.
  2. A `nohup sleep` job is killed on natural shell exit, and survives it when
     `sweep_on_exit = false`.
  3. After the server is SIGKILLed, a HUP-ignoring job survives until the next
     server start, then is swept, and its receipt ends up `Retired` with
     "exit status lost".
  4. A process that escaped with `setsid` is never signaled.
  5. A recorded sid whose pid was reused is skipped.
  6. On Linux, the pidfd kill path is checked under a simulated race.
- **Windows:**
  1. Descendants die when the job handle drops.
  2. The job is swept after a natural exit.
  3. The exit code is preserved.
- **Resource check:** with N panes open, there are exactly N shell processes
  and no helpers.

**Commands:**

1. `cargo clippy --all-targets -- -D warnings` (plus the `bmux_cli` feature
   matrix)
2. `cargo nextest run --no-fail-fast`
3. `cargo machete --with-metadata`
4. `bash scripts/check-workspace-dependencies.sh`
5. `cargo test -p bmux_cli`
6. `./scripts/smoke-pty-runtime.sh`
7. `./scripts/compat-matrix.sh`
8. `bmux plugin rebuild --all-workspace-plugins`

## Deviations from the proposal

- **Fatal signals.** The crash guard (`runtime/crash_guard.rs`) hooks only
  `SIGABRT` (which also covers `panic = "abort"` and Rust's stack-overflow
  abort) plus an `atexit` hook, not `SIGSEGV`/`SIGBUS`/`SIGILL`/`SIGFPE`.
  wasmtime handles guest traps through those signals on Linux; a handler in
  front of it would kill every pane on an ordinary trap. Faults are covered by
  the startup sweep instead.
- **Survivors with `sweep_on_exit = false`.** After the shell exits, the pane
  keeps serving its terminal while left-running jobs hold it, and closing the
  pane sweeps them. Once the terminal reaches EOF the session is retired and
  remaining jobs are released. On macOS this can happen right after the shell
  exits (observed in roughly one run in four): the kernel can detach the
  terminal from the session when its leader exits.
- **Readiness.** The old helper checks are gone; `execution_ready` is false only
  when the activation's executable identity cannot be attested (managed builds)
  or the runtime is fenced. The automatic-restart blocker now looks for live
  crash records under `<runtime_dir>/pane-sessions/`.

## Decisions

All decided on 2026-11-21:

1. **Natural-exit sweep.** Background and `nohup` jobs are killed by default.
   `[plugins.settings."bmux.pane_runtime".execution] sweep_on_exit = false`
   turns this off.
2. **macOS PID-reuse window.** Accepted and documented. Linux uses pidfd, so it
   has no window.
3. **Crash-recovered receipts.** Marked `Exited` ("exit status lost") and
   retired automatically during startup recovery.
4. **Auxiliary-executable mechanism.** Deleted. The shared hashing/retention
   primitives and the inventory tolerance stay.
5. **Readiness API.** `helper_ready`/`helper_error` become
   `execution_ready`/`execution_error`, and the in-repo consumers are updated.
