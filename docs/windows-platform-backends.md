# Windows platform backends

## Scope and ownership

This change introduces native backend boundaries, not a complete rewrite of the
core architecture. Execution policy remains in the pane-runtime plugin;
recording
owns its interactive frontend and storage. Shared code consumes backend-neutral
operations, while OS handles, signals, ACLs, environment encoding, and
durability
mechanisms live in platform modules. No new domain convenience methods belong in
`HostRuntimeApi` or the generic plugin host.

## Pane execution

The server spawns each pane's program directly on its ConPTY. There is no
helper process. `runtime/session_control.rs` is the platform-neutral owner;
`session_control/windows.rs` is the Windows backend:

- `prepare` creates an unnamed, non-inheritable Job Object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and no breakaway limits.
- `bmux_pty::PtySlave::spawn_in_job` creates the program with both
  `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and `PROC_THREAD_ATTRIBUTE_JOB_LIST`, so
  it is in the job from its first instruction and nothing it starts can escape.
  Exact environments (`SpawnCommand::exact_environment`) are validated and
  ordinal-sorted; `cmd /C` scripts are passed verbatim with `/S`.
- A forced stop is `TerminateJobObject` followed by a bounded wait until the
  job's active-process count is zero; processes that remain are a cleanup
  error. Cooperative stop is unsupported and escalates to a forced stop.
- The server holds the job handle for the pane's lifetime. If the server dies,
  even when killed outright, the kernel closes the handle and kill-on-close ends
  every process in the job. No crash records are needed on Windows.
- With `sweep_on_exit = false`, jobs left when the shell exits stay in the job,
  kill-on-close still set, while they hold the console: a server crash ends
  them, as the recovery sweep does on Unix. Kill-on-close is cleared only when
  the pane releases them, so releasing the handle leaves them running.

The early ConPTY cursor reply (`native_process/conpty_startup.rs`) is unchanged.
The console host is not in the job; the retired console is closed explicitly.

## Environment and interactive frontend

`runtime/environment.rs` keeps common entry-count/byte budgets and duplicate-key
validation. Native codecs are selected as whole modules:

- Unix uses `unix-bytes`; absent encoding is accepted as that legacy format.
- Windows requires `windows-utf16le`, preserves native UTF-16 code units, and uses
  Windows case-insensitive key identity (including native drive-directory keys).

The `windows-utf16le` / `unix-bytes` encoding tag identifies the native
representation. Incompatible,
malformed, duplicate, or oversized input is rejected rather than lossily
decoded.

Recording's shared `interactive_command.rs` and `interactive_terminal.rs` use
`platform.rs`. Unix signal registrations and Windows console stop streams belong
to the native frontend backends; environment collection and executable checks
also remain there. Storage uses platform-specific private creation, locking,
flushing, and namespace publication. See
`plugins/recording-plugin/NATIVE_STORAGE.md` for that contract. A frontend stop
retains the execution instead of being interpreted as successful cleanup.

## Browser hosting and compiler stack

`packages/web-host` shares the real bounded HTTP/authentication, publication,
viewport, and shutdown implementation across Unix and Windows. Asset/cache
access is isolated under `src/platform/`:

- Unix keeps handle-relative `openat`/`O_NOFOLLOW`, `flock`, private modes, and
  directory synchronization.
- Windows retains local-drive ancestor handles, performs relative NT opens, and
  rejects reparse points (including junctions), alternate-stream components, and
  unsupported UNC/device paths. Cache state uses current-user private DACLs and
  a kernel file lock. Files are flushed; publication uses a write-through,
  non-replacing move. Corrupt or preexisting conflicting cache state is an error,
  not permission to overwrite it or pretend the installation is empty.

Windows supports verified release bundles, verified offline caches, and explicit
asset directories. **Native Windows source-first-use asset generation is not
implemented**: it needs a pinned native setup driver. The existing source driver
uses Unix Nix/Bash. The Windows backend returns a precise unsupported error
instead of executing an ambient unpinned compiler. This does not disable web
hosting or the plugin.

Mermaid AOT compilation uses a joined native worker with an explicit 16 MiB
compiler stack; this is separate from guest execution stack limits. Destination
configuration, fuel variants, AOT/JIT identity checks, and compiler failure
propagation remain intact. No global compiler environment/stack override is
used.

## Mechanical guard and review scope

Run:

```sh
python3 scripts/check-platform-boundaries.py
python3 scripts/test-platform-boundaries.py
```

The guard scans the named shared session-control, environment, and interactive
frontend files on every host. It rejects raw OS namespaces/crates/handles,
native signal calls, ABI declarations, and platform cfg on implementations.
Whole out-of-line module selectors and explicit positive `cfg(test)` items are
allowed. Comments/literals are masked. Missing files fail closed. It is a
focused
lexical regression guard, not a Rust parser, authorization audit, dependency
graph
proof, or behavioral test; OS backend safety still requires native tests/review.

For this working diff, the required core-layer review found changes only in
`packages/ipc/src/transport.rs` and its new `transport/{unix,windows}.rs`
modules
among the enumerated core layers. These introduce generic endpoint connection
and
named-pipe instance ownership behavior, not tabs, sessions, contexts,
clients, panes, or permissions domain orchestration. The reviewed diff does not
change `HostRuntimeApi`. This statement is diff-scoped, not certification that
all preexisting core code is domain-free. Recheck added lines if scope changes.

## Validation status

The parent reported the native Windows production build passing and web-host
native tests passing (78 passed, one intentionally ignored). Full workspace
failures were still under investigation at this checkpoint. These results do not
establish that all tests pass, Unix behavior is fully regression-free, or every
Windows capability has parity. The guard tests establish only the documented
syntactic boundary; the parent retains responsibility for full validation and
native integration evidence.
