# bmux_session

Session management for bmux terminal multiplexer.

## Overview

This package manages bmux sessions, providing functionality to create, modify, and destroy session instances with their associated windows and panes.

## Features

- Session lifecycle management
- Session state persistence
- Window and pane organization
- Session metadata tracking

## Core Components

- **SessionManager**: Central session orchestration
- **Session**: Individual session instances
- **SessionInfo**: Session metadata and status

## Usage

```rust
use bmux_session::SessionManager;

let mut manager = SessionManager::new();
let session_id = manager.create_session(Some("my-session".to_string()))?;
```
