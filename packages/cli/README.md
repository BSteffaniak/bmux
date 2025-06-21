# bmux_cli

Command-line interface for bmux terminal multiplexer.

## Overview

This package provides the main command-line executable for bmux, handling user commands and interfacing with the bmux server.

## Features

- Session management commands
- Terminal attachment and detachment
- Configuration handling
- Server lifecycle management

## Commands

- `bmux new-session` - Create a new session
- `bmux attach` - Attach to an existing session
- `bmux list-sessions` - List all sessions
- `bmux kill-session` - Terminate a session
- `bmux server` - Server management

## Usage

```bash
bmux new-session --session my-session
bmux attach --target my-session
```
