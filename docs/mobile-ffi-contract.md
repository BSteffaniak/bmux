# Mobile FFI Contract

This document defines the API contract between platform apps and shared Rust.

Status: implemented for the paired-host model
(`docs/development/remote-access-architecture.md`, section 7). A phone is a
paired device like any other bmux client: it pairs with a host through the
host owner's `bmux remote pair` link and reaches saved hosts over Iroh (QUIC)
through the connections plugin's saved-host connector. The earlier
`ssh`/`tls`/`iroh` target model and its SSH host-key pinning APIs are gone; old
saved targets are not migrated, so users pair again.

## Design Principles

- Keep platform-facing APIs stable and coarse-grained.
- Keep platform-neutral types in FFI (no Android/iOS concepts).
- Return structured errors with machine-readable codes and user-safe messages.
- Reuse the desktop client code paths; the mobile crates add no protocol.

## Crates

- `packages/mobile-core`: mobile domain logic over
  `bmux_connections_plugin::paired_hosts` (pairing, saved hosts, connector) and
  `bmux_client` (attach).
- `packages/mobile-ffi`: UniFFI exports (`MobileApiFfi`).

## Storage

`MobileApiFfi(storage_dir)` takes one app-private directory. Every API object
of one app passes the same directory. Under it:

- `state/connections/devices/default.key`: this device's Iroh key (owner-only).
- `state/connections/known-hosts/default.json`: saved hosts with pinned host
  keys (owner-only, atomic replace).
- `config/bmux.toml` (optional): connections plugin settings such as
  `remote.relay` for self-hosted or disabled relays.

This is the layout of a desktop client profile. Android uses
`noBackupFilesDir/bmux`, so a restored backup never clones the device key onto
another phone.

## Host Model

`PairedHostFfi` (persisted by the core, carries no secret):

- `alias`: the name the host is saved under
- `host_key`: the pinned host key (Iroh endpoint id)
- `role`: `full` or `view_only`, as granted at pairing; the host enforces the
  current role, which its owner can change later
- `last_path`: `direct` or `relay`, from the last connection

## Core API Surface

Pairing:

- `start_pairing(link, alias?, device_name) -> PairingStateFfi`: checks the
  link, then dials `bmux/pair/1` in the background. One pairing runs at a
  time; starting another cancels the previous one.
- `pairing_state(pairing_id) -> PairingStateFfi`: `connecting`,
  `awaiting_approval` (with the `phrase` both devices show), `paired` (with the
  saved `host`), `failed` (with `error`), or `cancelled`. Reading it is local;
  the host pushes its decision over the open pairing stream.
- `cancel_pairing(pairing_id) -> PairingStateFfi`: drops the pairing
  connection, which withdraws the request on the host.

Saved hosts:

- `list_hosts() -> Vec<PairedHostFfi>`
- `rename_host(alias, new_alias) -> PairedHostFfi`
- `forget_host(alias)`: forgets the host on this device only; the host keeps
  the device until its owner runs `bmux remote revoke`.

Connections:

- `connect(host, session?) -> ConnectionStateFfi`: dials the saved host over
  the shared QUIC connection and completes the `Hello` handshake, so success
  means the host answered and still accepts this device.
- `disconnect(connection_id) -> ConnectionStateFfi`

Terminal streams (unchanged apart from addressing a host by alias):

- `open_terminal(TerminalOpenRequestFfi { host, session?, rows, cols })`
- `poll_terminal_presentation(terminal_id, max_chunks, version)`,
  `poll_terminal_output(terminal_id, max_chunks)`
- `write_terminal_input`, `send_terminal_mouse_event`, `resize_terminal`,
  `close_terminal`, `terminal_state`, `terminal_diagnostics`,
  `latest_terminal_failure`

Without an explicit session, `open_terminal` tries the session this host last
attached, then the session with the most clients, creating one when the host
has none.

## Error Model

All calls return `Result<T, MobileFfiError>`:

- `invalid_pairing_link`: the text is not a `bmux-pair:` link
- `pairing_not_found`: no such pairing in progress
- `host_not_found`: no saved host under that alias
- `host_store`: saved hosts could not be read or written, or an alias is
  invalid or taken
- `connection_failed`: unreachable host, changed host key, or a host that
  refuses this device (revoked or never paired); the message says which
- `invalid_argument`, `connection_not_active`, `terminal_session_not_found`,
  `terminal_session_closed`, `invalid_terminal_size`,
  `terminal_backend_failure`

## Backward Compatibility Rules

- Additive fields only for minor updates.
- Do not repurpose enum variants.
- New transport capabilities must be feature-detectable.

## Open Items

- Event streams instead of polling for pairing and connection state.
- Reconnect with resume inside the core (the Android foreground service retries
  `connect` with backoff today).
- iOS bindings and app.
