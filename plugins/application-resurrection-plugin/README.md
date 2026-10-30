# Application resurrection adapter

Default-bundled, independently disableable host adapter for the experimental
`application-resurrection/1` protocol. Applications exchange opaque session
references through a private per-launch Unix socket; BMUX does not interpret
application session IDs or execute commands from descriptors.

## Current implementation

- BPDL launch services prepare a socket, bind the spawned PID, process shell
  boundaries, and close the endpoint when the pane process exits.
- macOS native process metadata and Linux `/proc` authenticate process birth,
  session, terminal and foreground ownership in addition to socket peer PID/UID.
  Explicit launches require the bound process incarnation. Shell launches use the
  current registered shell invocation, including integrated nested shells and
  same-process `exec` replacement; endpoint inheritance alone grants nothing.
- Checkpoints are atomically replaced, file-synced and directory-synced before
  acknowledgement. Corrupt and unsupported checkpoints fail explicitly.
- A restoration offer survives the initial shell prompt and replay command marker.
  Subsequent prompt/command markers clear it and invalidate prior registrations.
- Disabling the plugin leaves ordinary launch and command replay available.
- Shell integration supports Bash, Zsh, Fish and Nushell. Nested Nushell and Fish
  discover per-launch vendor autoload hooks. Nested Bash and Zsh use the startup
  integration below. Explicit program launches do not need shell integration.
- Checkpoint v2 stores a bounded nested command path with distinct invocation IDs,
  shell executables, commands, working directories and application checkpoints.
  Existing v1 checkpoints remain readable. Old hosts reject v2 rather than
  silently lose a nested recovery path. Shell memory and arbitrary wrapper state
  are not reconstructed.

## Recovery and validation

Pane snapshots retain an opaque invocation identity. Before final snapshot
collection, the snapshot orchestrator quiesces every participant through the
generic
`StatefulPlugin::prepare_final_snapshot` hook. The pane-runtime participant
drains
application checkpoint writers while holding the shell-metadata lock and freezes
its recovery metadata; late prompts, command markers, process exit, and process
inspection cannot change the captured invocation. Normal saves do not freeze
live
state. A quiescence failure preserves the previous snapshot.

The v2 launch contract rejects
missing or mismatched referenced checkpoints rather than adopting another
command's
state. Shell metadata and identity transitions share a lock during snapshot
capture.
Interrupted transitions can therefore produce an explicit recovery error, not an
offer for the wrong invocation. Old snapshots without a checkpoint launch
normally;
old snapshots with an unassociated checkpoint fail without overwriting it.

Shell markers carry fresh per-launch tokens from private integration
directories.
This prevents ordinary output and stale recordings from impersonating shell
hooks;
same-user code able to read those files remains within the local trust boundary.
Peer UID/PID and foreground child ownership are checked separately.

`application_resurrection_integration` exercises real daemon restart for direct
launch and Bash replay, including a subsequent manual launch receiving no offer.
`bcode_resurrection_integration` runs the actual Bcode TUI with isolated state
and
no model requests. Its second launch omits the session argument and verifies
that
Bcode publishes the recovered session in the replacement generation. Run
explicitly:

```sh
BCODE_RESURRECTION_TEST_BINARY=/absolute/path/to/bcode \
  nix develop --command cargo test -p bmux_cli \
  --test bcode_resurrection_integration -- --ignored
```

The cross-repository test is ignored in the default suite because it requires a
separately built Bcode binary. Nested/non-exec wrappers and remote applications
are
not automatically adopted unless their shell boundaries are integrated. Non-exec
wrappers and ambiguous pipelines require an explicit recovery policy; they are
not inferred from process command lines. This experimental protocol is not an
established
cross-terminal standard.

Discovery is not authorization. No private credentials are persisted. Normal
application selection/opening remains the application's responsibility. Host
transport disconnect is not treated as process exit, and failed descriptor
persistence stops that endpoint rather than acknowledging uncertain state.

## Nested-shell startup integration

The ordinary BMUX executable handles lifecycle callbacks; no additional callback
binary or daemon is installed. Hooks are inactive outside a BMUX application
host. Nushell and Fish use a private vendor directory inherited through
`XDG_DATA_DIRS` (unless a child deliberately replaces that environment).

Bash does not have an inherited vendor autoload facility. To support ordinary
nested `bash` launches, add this to the end of `.bashrc`:

```bash
if [[ -n ${BMUX_SHELL_CALLBACK:-} && -n ${BMUX_ROOT_SHELL_PID:-} && $BMUX_ROOT_SHELL_PID != $$ ]]; then
  eval "$("$BMUX_SHELL_CALLBACK" __shell-integration bash)"
fi
```

For nested Zsh, add to `.zshrc`:

```zsh
if [[ -n ${BMUX_SHELL_CALLBACK:-} ]]; then
  eval "$("$BMUX_SHELL_CALLBACK" __shell-integration zsh)"
fi
```

BMUX does not edit user startup files. Its generated Zsh startup directory also
handles nested Zsh when that directory is inherited. Shells started without user
configuration cannot claim integrated command recovery.

Nested shells register over the same per-pane socket. The host verifies callback
ancestry and process birth, assigns a separate invocation to each command, and
persists it before acknowledging the boundary. A restored shell receives its
saved cwd, then signals readiness. The pane owner retrieves the pending command
through its typed lifecycle service: terminal output contains no executable
recovery command. Competing shell branches and application owners fail closed.
Only command replay is promised, not shell variables, functions or arbitrary
side-effect-free reconstruction. Existing shell replay policy still applies.

## Socket handshake failures

A successful `application-resurrection/1` hello receives the existing
`{"protocol":"application-resurrection/1","restore":null}` response (or a
restore
reference). A rejected hello receives a bounded response such as
`{"protocol":"application-resurrection/1","error":{"code":"ownership_rejected"}}`
before the connection closes. Success and rejection are mutually exclusive.
Rejections never contain descriptors, private paths, or raw operating-system
errors.

Codes are `invalid_handshake` (framing/protocol/peer identity),
`ownership_verification_unavailable` (process inspection failed),
`ownership_rejected` (the peer is not the current execution owner), and
`launch_unavailable` (launch or registration state is unavailable).
Clients must fail closed for rejections and unknown codes; older clients fail
their
welcome decoding, and newer clients still recognize EOF from older hosts.

Foreground inspection uses bounded native process reads rather than spawning
`ps`. A bounded grace period permits a newly spawned application to acquire its
foreground process group. Inspection failure never grants ownership.
