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

- **`permission-query/v1`** -- `list` permissions for a session
- **`permission-command/v1`** -- `grant` / `revoke` roles
- **`session-policy-query/v1`** -- `check` / `list-hot-path-overrides` / `resolve-hot-path-decision`
- **`session-policy-command/v1`** -- `grant-hot-path-override` / `revoke-hot-path-override`
