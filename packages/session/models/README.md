# bmux_session_models

Data models and types for bmux session management.

## Overview

This package defines the core data structures and types used throughout the bmux session management system.

## Features

- Session data model definitions
- Window and pane identifiers
- Error types for session operations
- Serialization support

## Core Types

- **SessionId**: Unique session identifier
- **WindowId**: Unique window identifier
- **PaneId**: Unique pane identifier
- **SessionError**: Session-specific error types

## Usage

```rust
use bmux_session_models::{SessionId, WindowId, PaneId};

let session_id = SessionId::new();
let window_id = WindowId::new();
```
