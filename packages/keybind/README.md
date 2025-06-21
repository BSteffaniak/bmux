# bmux_keybind

Keybinding management for bmux terminal multiplexer.

## Overview

This package handles keybinding configuration, key sequence processing, and custom key mapping functionality.

## Features

- Keybinding configuration
- Key sequence parsing
- Custom key mapping
- Context-sensitive bindings

## Core Components

- **KeybindManager**: Keybinding orchestration
- **KeySequence**: Key combination handling
- **BindingContext**: Context-aware bindings

## Usage

```rust
use bmux_keybind::KeybindManager;

let manager = KeybindManager::new();
// Keybinding operations
```
