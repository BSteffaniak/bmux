# bmux_client

Client-side communication for bmux terminal multiplexer.

## Overview

This package provides client-side functionality for communicating with the bmux server, handling connection management and message passing.

## Features

- Server connection management
- Protocol message handling
- Connection retry logic
- Session attachment/detachment

## Core Components

- **BmuxClient**: Main client interface
- **Connection**: Server connection handling
- **MessageHandler**: Protocol message processing

## Usage

```rust
use bmux_client::BmuxClient;

let client = BmuxClient::connect(socket_path)?;
// Client operations
```
