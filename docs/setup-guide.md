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
