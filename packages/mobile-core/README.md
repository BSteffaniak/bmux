# mobile-core

Shared Rust domain core for bmux mobile clients.

This crate provides shared Rust domain primitives for bmux mobile clients.

Current responsibilities:

- target parsing and canonicalization
- transport-agnostic connection state machine
- iroh/tls/ssh transport adapters
- session metadata APIs for connection-first mobile flows
- terminal stream session APIs (`open/poll/write/resize/close`)
- typed diagnostic and error model for platform UIs

## Native pointer output v1

`ConnectionManager::poll_terminal_presentation(id, max_chunks, 1)` explicitly
selects typed pointer output. Unknown versions fail before any bytes are
drained.
The existing byte-only API is unchanged. The presentation contains byte chunks
followed by optional complete `TerminalPointerOutput` metadata: attachment
`generation`, monotonic `revision`, and `Hidden` or a validated CSS shape.
Consumers must accept all preceding bytes before applying the metadata; it is
never emulator input. Unchanged pointer state is not repeatedly delivered.

The remote actor resolves through `attach_pipeline::resolve_pointer_shape` using
`retained_surfaces_from_attach_scene` and authoritative terminal-grid protocol
state. Snapshot/delta hydration explicitly uses the generated
`pane_runtime_pointer_grid_v1` service with `format_version=1`; unsupported
providers fail rather than retry a legacy representation that could lose stacks.
Failed initial hydration or repair terminates the stream explicitly. Hover and
leave use structured mouse events, independent of application
mouse reporting. Snapshot/delta repair uses the existing grid path; a reopened
backend has a fresh pointer generation. A full output queue now fails explicitly
rather than silently truncating ANSI data; reconnect is required after overflow.

Coverage is the mobile path's existing attach scene and pane content. It does
not
create plugin chrome or resize/floating gestures. CLI's owner-scoped plugin
snapshots live in an attach-local `PluginSurfaceRegistry`; this remote actor
does
not receive that registry's snapshots or allocation revisions. Extending
existing
mobile surfaces to plugin chrome requires carrying those complete snapshots and
using `retained_surfaces_from_plugin_surfaces` at the same accepted presentation
boundary, not recreating tab or hit-test policy in mobile. Content bounds are
provided by the shared attach-scene adapter, not mobile-specific geometry
policy.

See:

- `docs/mobile-m0-architecture.md`
- `docs/mobile-ffi-contract.md`
