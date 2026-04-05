# bmux_ipc

Wire protocol, framing, transport, and compression for bmux IPC.

## Overview

Defines the complete client-server wire protocol for bmux. This is the shared
contract between the client and server crates -- all request/response types,
protocol version negotiation, frame encoding, transport abstraction, and
optional compression live here. The largest model crate in the workspace.

## Features

- Cross-platform IPC endpoints (Unix domain sockets, Windows named pipes)
- Protocol version negotiation with capability advertisement
- Length-delimited framing with CRC integrity checks
- Optional zstd and LZ4 frame compression (`compression` feature)
- Complete request/response envelope types for all server operations
- Session, pane, client, and context summary models
- Attach scene graph types (surfaces, layers, pane chunks)
- Recording profile and status types

## Core Types

- **`IpcEndpoint`**: `UnixSocket(PathBuf)` or `WindowsNamedPipe(String)`
- **`Envelope`**: Top-level wire message with `EnvelopeKind` discriminant
- **`Request`** / **`Response`**: Typed request and response payloads
- **`ProtocolContract`**: Advertised capabilities and version
- **`NegotiatedProtocol`**: Result of capability negotiation
- **`AttachGrant`**: Server-issued attach token with layout state
- **`AttachPaneChunk`**: Streamed pane output during attach

## Modules

- **`transport`**: `LocalIpcListener`, `LocalIpcStream`, `IpcStreamWriter`/`Reader`
- **`frame`**: Length-delimited frame encoding/decoding with optional compression
- **`compression`**: Pluggable zstd/LZ4 codecs behind feature flags
- **`compressed_stream`**: Streaming compression wrapper for `AsyncWrite`

## Usage

```rust
use bmux_ipc::{IpcEndpoint, encode, decode};
use bmux_ipc::transport::LocalIpcListener;

let endpoint = IpcEndpoint::from_session_name("my-session");
let listener = LocalIpcListener::bind(&endpoint).await?;
let stream = listener.accept().await?;
```
