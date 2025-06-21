# bmux_server

Server-side functionality for bmux terminal multiplexer.

## Overview

This package implements the bmux server, handling client connections, session management, and coordinating all server-side operations.

## Features

- Client connection handling
- Session orchestration
- Server lifecycle management
- Inter-process communication

## Core Components

- **BmuxServer**: Main server implementation
- **ClientHandler**: Individual client management
- **ServerState**: Global server state

## Usage

```rust
use bmux_server::BmuxServer;

let server = BmuxServer::new(socket_path)?;
server.run().await?;
```
