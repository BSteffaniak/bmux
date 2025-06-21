# bmux_terminal_models

Data models and types for bmux terminal management.

## Overview

This package defines the core data structures used for terminal, window, and pane management within bmux.

## Features

- Terminal size and layout models
- Pane and window data structures
- Split direction and layout types
- Builder patterns for configuration

## Core Types

- **PaneSize**: Terminal dimensions
- **PaneLayout**: Hierarchical pane arrangements
- **SplitDirection**: Horizontal/vertical splits
- **Pane**: Individual terminal pane
- **Window**: Container for multiple panes

## Usage

```rust
use bmux_terminal_models::{PaneSize, Window, SplitDirection};

let size = PaneSize::new(80, 24);
let window = Window::new(size, Some("my-window".to_string()));
``` 