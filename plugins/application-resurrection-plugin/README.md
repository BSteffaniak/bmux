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

## Validation and remaining hardening

Socket tests exercise real peer authentication, acknowledged persistence, restored
versus manual launch behavior, stale endpoints, and shell replay transitions.
The implementation still needs a full Bcode-in-BMUX restart test and review of
atomic association between the independent descriptor checkpoint and the pane
snapshot. In particular, interrupted command changes must not pair a newer pane
snapshot with an older descriptor. Shell marker provenance and foreground-process
ownership also need stronger validation before claiming a hardened protocol.

Discovery is not authorization. No private credentials are persisted. Normal
application selection/opening remains the application's responsibility. Host
transport disconnect is not treated as process exit, and failed descriptor
persistence stops that endpoint rather than acknowledging uncertain state.
