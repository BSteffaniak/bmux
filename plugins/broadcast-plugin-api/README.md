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

## Turnkey and safe presentation contracts

New capability `bmux.broadcast.turnkey.v1` / `broadcast_turnkey_v1` leaves both
existing v1 interfaces unchanged. Authenticate using the existing local proof
flow on the same persistent connection before any turnkey request.

- `start_profile(client, profile: Option<String>, source_id: String)` returns
  `Publication { token, local, remote, source_id, error_code, viewer_url }`.
  The source UUID must belong to that caller. The private token is teardown
  authority: never log it or publish it. Identical retries return the same token;
  conflicting/superseded attempts reject. Retain the connection and token.
- `stop_owned(client, token)` returns false for stale/foreign ownership and never
  stops a newer manual start/source selection.
- `status()` returns `BroadcastStatus` with all status fields above except token.
- `register(source_id, label)` returns `SourceDescriptor { source_id, label,
  selected }`; labels must be explicit printable ASCII, 1–64 bytes, never
  inferred private titles/paths. Renew the source lease via output-v1 polling.
- `sources()` returns bounded active descriptors.

`bmux.broadcast.presentation.v1` / `broadcast_presentation_v1::client::status`
is proof-free and returns only `Indicator { state, revision }`. States are
`live|paused|offline|unavailable`; starting/recovering never falsely claim live.
Reads never initialize the publisher or perform network IO. Consumers must
validate states/revisions outside rendering and clear/remove stale UI on loss.

## Committed producer contract

Use `broadcast_output_v1::client::{register, poll, publish, unregister}` on the
same authorized persistent connection. Register an explicit source UUID; the
server binds it to that connection's caller UUID. Register/poll renew a six-second
lease. Poll at least every two seconds and unregister when the producer stops.
Expired registrations can be explicitly replaced; polling cannot revive them.

`register` and `poll` return `SourceState { refresh: u64, selected: bool }`.
When selected, submit a self-contained full inline `BMWP\x05` frame with
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
