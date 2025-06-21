# bmux_terminal

Terminal management for bmux terminal multiplexer.

## Overview

This package handles terminal instances, process management, and terminal I/O operations within bmux sessions.

## Features

- Terminal process spawning and management
- Terminal I/O handling
- Process lifecycle management
- Terminal state tracking

## Core Components

- **TerminalManager**: Terminal orchestration
- **TerminalInstance**: Individual terminal processes
- **ProcessHandle**: Process management utilities

## Usage

```rust
use bmux_terminal::TerminalManager;

let mut manager = TerminalManager::new();
// Terminal management operations
```
