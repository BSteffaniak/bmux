# Broadcast plugin

Optional server-owned live broadcasting of selected committed audience output.
The controller persists across CLI invocations, while fresh plugin activation
starts offline. Relay failure cannot interrupt local presentation or terminal
operation. No terminal input or presenter navigation is exposed to viewers.

## Commands

```text
bmux broadcast start ENDPOINT CHANNEL CREDENTIAL_FILE SOURCE_UUID
bmux broadcast pause
bmux broadcast resume
bmux broadcast stop
bmux broadcast status
bmux broadcast source SOURCE_UUID
bmux broadcast terminal SOURCE_UUID SESSION_UUID ABSOLUTE_EXECUTABLE SHA256 VERSION COLS ROWS
```

Register an audience producer or terminal source before starting. `terminal`
requires the explicitly selected native executable's SHA-256 and exact
`--version` output; quote the version argument if it contains spaces. The worker
preserves the selected configuration/runtime/data/state paths and uses the
private, version-checked BMAO1 retained-output transport, not a CLI library or
screen reconstruction. Stop terminates owned terminal observers, not their
underlying sessions.

Endpoints require HTTPS except loopback development HTTP. Channels are bounded
lowercase alphanumeric/hyphen names. Publisher credentials are read from bounded,
owner-only regular files with symlinks rejected; never pass the key itself as an
argument. Requests have deadlines and redirects disabled. Diagnostic responses
use content-free error codes, never raw network errors or tokens.

## Local control and source handoff

The CLI connects to the selected BMUX runtime, calls `prepare_local`, reads its
local `broadcast-local-proof` file, and calls `authorize` with those bytes. Grants
bind the exact caller connection. An authenticated remote UUID cannot trigger
credential reads, executable launch, or producer registration without proof
possession. Deactivation revokes grants, cancels publishing and owned observers,
attempts remote stop, and drains worker threads.

Other local audience producers use the generated
[`bmux_broadcast_plugin_api`](../broadcast-plugin-api/README.md) clients on a
persistent connection. Source polling renews a six-second lease and observes
controller-issued refresh generations. Resume and source changes require a
fresh complete repair even for an unchanged scene. Only committed retained
output is handed off; private notes and controls are not audience content.

## Delivery guarantees

The controller keeps one latest pending full inline BMWP2 repair, limited to
8 MiB. HTTP mutation execution is serialized with pause/stop, and local gate
changes invalidate pending and in-flight uploads. Transient uploads retry exact
revision/bytes until acknowledged or superseded. Lost relay epochs reacquire
only under running local intent, invalidate prior output, and require a fresh
producer repair. Pause, stop, and unavailable sources never silently resume.

Status distinguishes local state from acknowledged remote state. In particular,
`stop_unacknowledged` means local uploads have stopped but remote revocation is
not confirmed; the relay lease bounds exposure when unreachable.

See [the HTTP contract](../../docs/live-broadcast-protocol.md) and the
[local broadcasting guide](../../docs/live-broadcasting.md) for integration.
