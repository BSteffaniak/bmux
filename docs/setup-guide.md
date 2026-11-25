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

## Remote Access Quick Start

Reach this machine's sessions from another device. On the host, enable remote
access once and print a one-time pairing link:

```bmux-cli
bmux remote enable
bmux remote pair
```

On the other device, run `bmux connect --pair` and paste the link. Both sides
show a phrase; approve on the host only when they match. The device saves the
host under a name and attaches; afterwards it reconnects by that name:

```bmux-cli
bmux connect --pair
bmux connect desk
```

See the full guide: [Remote Access and Pairing](/docs/remote-pairing)

## SSH Kiosk Setup

For locked SSH access flows, configure kiosk profiles and generate sshd/wrapper
artifacts:

```bmux-cli
bmux kiosk status
bmux kiosk init --all-profiles --dry-run
bmux kiosk init --all-profiles
bmux kiosk issue-token demo
```

See the full guide: [Kiosk Access](/docs/kiosk)

## Baseline Config Example

```bmux-config
[general]
scrollback_limit = 10000
```
