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

## Core types

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
