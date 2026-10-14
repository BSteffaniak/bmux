# Broadcast plugin API

Stable BPDL contracts for the optional output-only live broadcaster. This crate
contains generated types and clients only; controller state, filesystem access,
HTTP requests, and subprocess ownership live in `bmux_broadcast_plugin`.

## Local authorization

Use `broadcast_control_v1::client::prepare_local` on a persistent BMUX connection.
It initializes the serving plugin's local capability and returns only a boolean,
never a secret or filesystem path. The caller independently reads the 32-byte,
owner-only `broadcast-local-proof` file in its selected `ConfigPaths.runtime_dir`,
then passes those bytes to `client::authorize` on the same connection. Proof
possession authorizes that exact host-bound caller UUID; a UUID or plugin name
alone does not authorize control. Grants are bounded and expire after five
minutes without authorized activity. Plugin deactivation revokes them.

Never forward the proof to a relay, put it in command arguments, or log it.
All remaining control and producer operations require this local authorization.

## Controller contract

`broadcast_control_v1` exposes `start`, `pause`, `resume`, `stop`, `status`,
`source`, and `terminal`. `start` explicitly selects an endpoint, channel,
private publisher-credential file, and registered source UUID. `terminal`
registers a retained terminal source using an explicit executable path, SHA-256,
version, target UUID, and geometry; it does not choose an executable from PATH.

`Status` separates `local` intent from `remote` acknowledgement and includes the
selected `source_id` and an optional content-free `error_code`. Local stop closes
the upload gate before remote acknowledgement; `stop_unacknowledged` does not
claim that a remote frame has already disappeared. Startup is offline, and
local rendering never depends on relay availability.

## Committed producer contract

Use `broadcast_output_v1::client::{register, poll, publish, unregister}` on the
same authorized persistent connection. Register an explicit source UUID; the
server binds it to that connection's caller UUID. Register/poll renew a six-second
lease. Poll at least every two seconds and unregister when the producer stops.
Expired registrations can be explicitly replaced; polling cannot revive them.

`register` and `poll` return `SourceState { refresh: u64, selected: bool }`.
When selected, submit a self-contained full inline `BMWP\x02` frame with
`publish(source_id, refresh, frame)`. Frames are limited to 8 MiB and must come
from accepted retained audience output, never speaker notes or reconstructed
terminal state. Within a refresh, source revisions increase monotonically.

Pause, resume, source switching, generation transitions, and epoch recovery
invalidate pending work. Whenever the selected refresh changes, resubmit the
current committed full frame even if the slide or scene did not change. A
`false` publish result means it was not admitted; poll again rather than
assuming delivery. A `true` result admits only bounded local pending output,
not remote acknowledgement. The controller serializes relay mutations and
retains at most one latest pending repair, retrying transient failures without
requiring a scene change. Lease loss closes the gate and invalidates content.

See [the shared HTTP protocol](../../docs/live-broadcast-protocol.md) for relay
lease, epoch, revision, and renderer compatibility requirements.
