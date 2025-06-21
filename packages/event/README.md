# bmux_event

Event handling system for bmux terminal multiplexer.

## Overview

This package provides the core event processing system, handling keyboard input, mode transitions, and event routing throughout the application.

## Features

- Event processing and routing
- Mode management (Normal, Insert, Visual, Command)
- Keyboard input handling
- Event filtering and transformation

## Event Types

- **KeyEvent**: Keyboard input events
- **ModeTransition**: Mode change events
- **SystemEvent**: Internal system events

## Usage

```rust
use bmux_event::EventHandler;

let mut handler = EventHandler::new();
let result = handler.handle_key_event(key_event)?;
```
