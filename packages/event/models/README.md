# bmux_event_models

Data models and types for bmux event handling.

## Overview

This package defines the event system data structures including keyboard events, mode transitions, and system events.

## Features

- Keyboard event modeling
- Mode transition definitions
- Key modifier handling
- Event serialization

## Core Types

- **KeyEvent**: Keyboard input events
- **Mode**: Application modes (Normal, Insert, Visual, Command)
- **KeyModifiers**: Modifier key states
- **Event**: General event enumeration

## Usage

```rust
use bmux_event_models::{KeyEvent, Mode, KeyModifiers};

let event = KeyEvent::new('a', KeyModifiers::default());
let mode = Mode::Normal;
```
