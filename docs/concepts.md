# BMUX Concepts

This page gives a practical mental model for how bmux is structured so command
choices and troubleshooting steps are easier to reason about.

## Core Objects

- **Server**: long-lived control process that owns runtime state.
- **Session**: logical workspace you attach to.
- **Context**: generic attachable execution resource used by plugins.
- **Pane**: terminal surface executing shell/program I/O.
- **Client**: one attached viewer/controller with its own view state.

## Architecture Boundary

BMUX core is domain-agnostic. Windows and permissions are plugin domains.
Core runtime behavior should stay generic, and plugins should carry domain
logic through plugin/service interfaces.

## Command Surfaces

- **Task-first commands**: `bmux connect`, `bmux setup`, `bmux host`
- **Grouped commands**: `bmux session ...`, `bmux server ...`, `bmux remote ...`
- **Automation commands**: `bmux playbook ...`

## Quick Validation Examples

```bmux-cli
bmux setup --check
bmux server status --json
bmux list-sessions --json
```
