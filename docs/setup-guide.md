# Setup Guide

Step-by-step setup paths for common environments.

## Local Development Setup

1. Build the workspace.
2. Start a local server runtime.
3. Create and attach to a session.

```bmux-cli
bmux server start --daemon
bmux new-session dev
bmux attach dev
```

## Hosted Quick Start

```bmux-cli
bmux setup
bmux host --status
```

## SSH Kiosk Setup

For locked SSH access flows, configure kiosk profiles and generate sshd/wrapper artifacts:

```bmux-cli
bmux kiosk status
bmux kiosk init --all-profiles --dry-run
bmux kiosk init --all-profiles
bmux kiosk issue-token demo
```

See the full guide: [Kiosk Access](/docs/kiosk)

## Baseline Config Example

```bmux-config
[connections]
default_target = "local"

[connections.targets.prod]
transport = "ssh"
host = "prod.example.com"
user = "bmux"
port = 22
```
