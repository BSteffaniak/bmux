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

## Managed execution

`plugins/pane-runtime-plugin/src/runtime/supervisor.rs` owns helper installation
hash verification, admission, authenticated requests, startup authorization, and
lifecycle snapshot validation. `supervisor/unix.rs` and `supervisor/windows.rs`
implement the native transport/spawn/lifetime mechanisms. The shared entry point
is `spawn_terminal(master, slave, command)`; lifecycle readers expose a
backend-neutral `LifecycleInterrupter::interrupt()`.

- Unix resolves the PTY slave through `master.tty_name()` and launches the
  guardian. Inherited readiness IPC and its unreaped startup-child guard remain
  Unix-owned. Failed negotiation kills/reaps the startup child. Lifetime
  installation and sticky revocation share one lock.
- Windows attaches the helper to ConPTY using `slave.spawn_command(command)`.
  Its invocation is `--guardian-windows-session-v1 BOOTSTRAP_PATH PIPE_NAME`.
  The random `\\.\pipe\bmux-exec-UUID` endpoint uses authenticated bounded
  framing, not a TCP fallback. Bootstrap creation uses current-user private ACLs.
  The client uses overlapped named-pipe handles for deadlines/cancellation.
- Private protocol version 8 adds `BindLifetime`. Windows establishes a dedicated
  connection, accepts an authenticated successful response, and retains that
  lease. EOF ends the helper's owned lifetime. Failed binding retains startup
  child cleanup ownership; revocation cannot be undone by a late install.
- `Subscribe` streams snapshots on both systems. Capability, protocol version,
  execution token, and increasing revision must match; EOF/interruption is not a
  successful terminal-end receipt. Requests and lifecycle receipts remain
  distinct from capture completion.

The Windows helper creates a suspended target, assigns it to its owned
kill-on-close Job Object, then resumes it. Job termination/active-process
accounting supplies the Windows cleanup boundary. This is not a POSIX process
group/session emulation, and numeric PID signaling is not a substitute for owned
handles. Unix session/process-group inspection and signaling stay in Unix helper
and native-process modules.

Windows cooperative/graceful `Term` is explicitly unsupported. Managed teardown
uses the bounded owned-job `Sweep` path; direct pane process signaling is also
unsupported there. Console Ctrl-C/close notifications received by an interactive
frontend do **not** grant authority to terminate the managed execution.

## Environment and interactive frontend

`runtime/environment.rs` keeps common entry-count/byte budgets and duplicate-key
validation. Native codecs are selected as whole modules:

- Unix uses `unix-bytes`; absent encoding is accepted as that legacy format.
- Windows requires `windows-utf16le`, preserves native UTF-16 code units, and uses
  Windows case-insensitive key identity (including native drive-directory keys).

`Start.native_env_encoding` identifies the native representation. Incompatible,
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

The guard scans the four named shared supervisor, environment, and interactive
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
