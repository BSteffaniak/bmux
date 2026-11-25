# bmux_permissions_plugin

Bundled permissions plugin for bmux.

## Overview

Implements role-based access control for bmux sessions. Each connected client
can be assigned a role (owner, writer, observer) that controls what operations
they are allowed to perform. Permission state is persisted via the host storage
API. Also provides session policy evaluation with hot-path override support for
latency-sensitive permission checks.

## Commands

- `permissions list <session>` -- list per-client roles for a session
- `permissions grant <session> <client> <role>` -- assign a role to a client
- `permissions revoke <session> <client>` -- remove a client's role

## Services

- **`permissions-state`** -- `list` permissions for a session
- **`permissions-commands`** -- `grant` / `revoke` roles
- **`session-policy-state`** -- `check` / `list-hot-path-overrides` / `resolve-hot-path-decision`
- **`session-policy-commands`** -- `grant-hot-path-override` / `revoke-hot-path-override`
- **`remote-role-commands`** / **`remote-role-state`** (`owner-local`) -- set,
  clear and read the role of a remote principal (a paired device): `full` or
  `view-only`

## Remote access

At activation the plugin registers a `RemoteAccessAuthorizerHandle` in the
host state registry. The server asks it for every request on an admitted
(remote) stream: `full` allows `observe`, `input` and `manage` routes,
`view-only` allows `observe`, and nothing allows `owner-local` routes. A
principal without a role gets nothing. Roles are held in memory and persisted
through plugin storage; the connections plugin sets them when the owner
approves a device or runs `bmux remote role`.
