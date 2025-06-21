# bmux_history

Command and session history management for bmux.

## Overview

This package manages command history, session history, and provides search and replay functionality for bmux sessions.

## Features

- Command history tracking
- Session history management
- History persistence
- Search and filtering capabilities

## Core Components

- **HistoryManager**: Central history management
- **CommandHistory**: Command tracking
- **SessionHistory**: Session state history

## Usage

```rust
use bmux_history::HistoryManager;

let mut history = HistoryManager::new();
// History operations
```
