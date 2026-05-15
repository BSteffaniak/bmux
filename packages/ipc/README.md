# bmux_ipc

Cross-platform IPC protocol models and transport primitives for bmux.

## Overview

This crate defines the core wire envelope used between bmux clients and the
server: endpoint addressing, protocol negotiation, request/response envelopes,
event delivery, service invocation, framing, transport, and optional
compression. Product-domain models and behavior live in plugin API, protocol,
or neutral state crates; IPC carries encoded payloads and generic routing data
for those domains.

## Responsibilities

- Represent Unix socket and Windows named-pipe endpoints.
- Negotiate wire epoch, protocol revision, and supported capabilities.
- Encode and decode top-level request/response/event envelopes.
- Route generic service invocations by capability, interface, and operation.
- Support service pipelines with encoded or JSON-template payloads.
- Provide local transport and length-delimited frame helpers.
- Offer optional payload/transport compression behind feature flags.

## Core types

- **`IpcEndpoint`**: Cross-platform local server endpoint.
- **`Envelope`** and **`EnvelopeKind`**: Top-level framed wire message.
- **`Request`** and **`ResponsePayload`**: Core control, event, and generic
  service transport payloads.
- **`ProtocolContract`** and **`NegotiatedProtocol`**: Capability and protocol
  compatibility negotiation.
- **`InvokeServiceKind`**, **`ServicePipelineRequest`**, and related service
  pipeline types: generic dispatch vocabulary for plugin/service calls.

## Modules

- **`transport`**: `LocalIpcListener`, `LocalIpcStream`, and async reader/writer
  traits.
- **`frame`**: Length-delimited frame encoding and decoding.
- **`compression`**: zstd/LZ4 payload compression support behind feature flags.
- **`compressed_stream`**: Streaming compression wrappers for async I/O.

## Usage

```rust,no_run
use bmux_ipc::transport::LocalIpcListener;
use bmux_ipc::IpcEndpoint;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let endpoint = IpcEndpoint::unix_socket("/tmp/bmux.sock");
let listener = LocalIpcListener::bind(&endpoint)?;
let stream = listener.accept().await?;
# let _ = stream;
# Ok(())
# }
```

Domain-specific callers should prefer typed plugin API crates for request and
response payloads, using IPC only as the core transport and service-routing
layer.
