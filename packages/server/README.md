# bmux_server

Core server host for bmux IPC and plugin services.

## Overview

This crate hosts the bmux server event loop and core IPC request handling. It
owns generic server plumbing such as endpoint binding, protocol negotiation,
event delivery, service dispatch, and shutdown coordination. Product-domain
behavior is implemented by plugins or neutral state crates and is reached
through generic request/service paths.

## Responsibilities

- Bind a local IPC endpoint and accept client connections.
- Negotiate protocol contracts with clients.
- Handle core control requests such as ping, status, event subscription, and
  server shutdown.
- Route generic service invocations to registered plugin/service handlers.
- Broadcast wire events to polling or server-push clients.
- Provide host-side runtime plumbing for recording, performance, and plugin
  state replayers without embedding domain convenience APIs.

## Core types

- **`BmuxServer`**: Server handle with explicit-endpoint and config-derived
  constructors.
- **`ServiceRoute`**: Capability/interface/operation route for service dispatch.
- **`ServiceInvokeContext`**: Per-call context available to service handlers.
- **`ServiceInvokeOutput`**: Encoded service response payload plus metadata.

## Usage

```rust,no_run
use bmux_ipc::IpcEndpoint;
use bmux_server::BmuxServer;

# async fn example() -> anyhow::Result<()> {
let endpoint = IpcEndpoint::unix_socket("/tmp/bmux.sock");
let server = BmuxServer::new(endpoint);

server.run().await?;
# Ok(())
# }
```

Callers that already have resolved bmux paths can use
`BmuxServer::from_config_paths` to derive the platform-specific endpoint from
configuration.
