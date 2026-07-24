# bmux_client

Client-side IPC facade for bmux.

## Overview

This crate provides the typed client used by the CLI, tests, and other host-side
callers to connect to a bmux server endpoint, complete protocol negotiation, and
send core IPC requests. Domain behavior is reached through generic service
invocation paths and typed plugin API crates rather than domain-specific client
helpers.

## Responsibilities

- Connect to a local or bridged bmux IPC endpoint.
- Negotiate the IPC protocol contract and supported capabilities.
- Encode requests, decode responses, and surface typed client errors.
- Provide generic helpers for service invocation and event delivery.
- Preserve caller principal identity for server-side policy checks.
- Resolve attach targets through registered providers before acquiring a legacy
  fallback client, allowing alternate providers to open independently.
- Validate native provider snapshots and deltas before rendering, preserve
  generation-scoped resume cursors across recoverable disconnects, route
  focused input/viewport/action operations generically, and always detach.
- Preserve the existing pane-runtime attach path as the default provider for
  bare and `local://` targets.
- Pool handshaken endpoint clients behind global and per-endpoint hard limits.
- Apply deadline-aware admission backpressure and retain bounded idle clients.
- Convert pooled clients into dedicated streaming leases whose capacity remains
  charged for the stream lifetime.

## Core types

- **`AttachProviderRegistry`**: Process-wide, scheme-neutral attach provider
  registration and deterministic target resolution with opaque provider plans.
- **`AttachSession`**: Object-safe native provider session contract for complete
  snapshots, cancellation-safe ordered events, input, viewport updates, generic
  actions, recoverable resume, and idempotent detach.
- **`AttachContinuityValidator`** and **`AttachControlValidator`**: Enforce
  event-sequence continuity, scene revisions, generation-scoped stream cursors,
  repair snapshots, resumable state, command ordering, action deduplication,
  focused-surface routing, and one-way detach.
- **`AttachProviderSession`**: Neutral legacy-client or native-session backend
  returned by providers to the attach runtime.
- **`EndpointConnectionPool`**: Domain-neutral handshaken-client pool with
  bounded global/per-endpoint admission, idle reuse, unhealthy discard, and
  dedicated streaming leases.
- **`BmuxClient`**: Stateful IPC client with request/response helpers.
- **`ClientError`**: Error type for transport, serialization, timeout, protocol,
  and server response failures.
- **`AttachOpenInfo`**, **`AttachSnapshotState`**, and related attach structs:
  host-side data returned by attach-oriented IPC/service workflows.
- **`ServerStatusInfo`** and **`PrincipalIdentityInfo`**: core server/control
  status responses.

## Usage

```rust,no_run
use std::time::Duration;

use bmux_client::BmuxClient;
use bmux_ipc::IpcEndpoint;

# async fn example() -> bmux_client::Result<()> {
let endpoint = IpcEndpoint::unix_socket("/tmp/bmux.sock");
let mut client = BmuxClient::connect(&endpoint, Duration::from_secs(5), "example").await?;

client.ping().await?;
# Ok(())
# }
```

For profile-aware callers, prefer `BmuxClient::connect_with_paths` or
`BmuxClient::connect_default` so the endpoint, timeout, and principal identity
come from the resolved bmux configuration.
