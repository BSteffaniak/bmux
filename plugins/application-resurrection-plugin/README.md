# Application resurrection adapter

Default-bundled, independently disableable host adapter for the experimental
`application-resurrection/1` protocol. Applications exchange opaque session
references through a private per-launch Unix socket; BMUX does not interpret
application session IDs or execute commands from descriptors.

## Current implementation

- BPDL launch services prepare a socket, bind the spawned PID, process shell
  boundaries, and close the endpoint when the pane process exits.
- macOS `LOCAL_PEERPID` and Linux `SO_PEERCRED` authenticate the connecting PID.
  Explicit launches require an exact PID match. Shell launches currently allow
  only an immediate child of the shell; nested applications and wrappers are not
  automatically adopted.
- Checkpoints are atomically replaced, file-synced and directory-synced before
  acknowledgement. Corrupt and unsupported checkpoints fail explicitly.
- A restoration offer survives the initial shell prompt and replay command marker.
  Subsequent prompt/command markers clear it and invalidate prior registrations.
- Disabling the plugin leaves ordinary launch and command replay available.
- Shell integration is limited to Bash, Zsh, Fish and Nushell. Other shells do not
  receive discovery. Explicit program launches do not need shell integration.

## Recovery and validation

Pane snapshots retain an opaque invocation identity. Before final snapshot
collection, the snapshot orchestrator quiesces every participant through the generic
`StatefulPlugin::prepare_final_snapshot` hook. The pane-runtime participant drains
application checkpoint writers while holding the shell-metadata lock and freezes
its recovery metadata; late prompts, command markers, process exit, and process
inspection cannot change the captured invocation. Normal saves do not freeze live
state. A quiescence failure preserves the previous snapshot.

The v2 launch contract rejects
missing or mismatched referenced checkpoints rather than adopting another command's
state. Shell metadata and identity transitions share a lock during snapshot capture.
Interrupted transitions can therefore produce an explicit recovery error, not an
offer for the wrong invocation. Old snapshots without a checkpoint launch normally;
old snapshots with an unassociated checkpoint fail without overwriting it.

Shell markers carry fresh per-launch tokens from private integration directories.
This prevents ordinary output and stale recordings from impersonating shell hooks;
same-user code able to read those files remains within the local trust boundary.
Peer UID/PID and foreground child ownership are checked separately.

`application_resurrection_integration` exercises real daemon restart for direct
launch and Bash replay, including a subsequent manual launch receiving no offer.
`bcode_resurrection_integration` runs the actual Bcode TUI with isolated state and
no model requests. Its second launch omits the session argument and verifies that
Bcode publishes the recovered session in the replacement generation. Run explicitly:

```sh
BCODE_RESURRECTION_TEST_BINARY=/absolute/path/to/bcode \
  bash scripts/dev.sh cargo test -p bmux_cli \
  --test bcode_resurrection_integration -- --ignored
```

The cross-repository test is ignored in the default suite because it requires a
separately built Bcode binary. Nested/non-exec wrappers and remote applications are
not automatically adopted. This experimental protocol is not an established
cross-terminal standard.

Discovery is not authorization. No private credentials are persisted. Normal
application selection/opening remains the application's responsibility. Host
transport disconnect is not treated as process exit, and failed descriptor
persistence stops that endpoint rather than acknowledging uncertain state.
