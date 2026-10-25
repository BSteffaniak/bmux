# Local IPC security policies

`bmux_ipc::transport` separates reusable Unix transport mechanisms from the
consumer's security policy. `LocalIpcListener::bind` and
`LocalIpcStream::connect` retain strict defaults. BMUX local runtime setup
explicitly selects `trusted_ancestors: false`, retaining all other strict
checks.
There is no environment-controlled transport-policy relaxation. This supports
private runtimes below writable external-volume ancestors without relocating
endpoints, but cannot prevent ancestor-based path substitution.

Consumers may use `LocalIpcListener::bind_with_policy` with
`UnixSecurityPolicy` and `LocalIpcStream::connect_with_policy` with an
`IdentityPolicy`. Each listener retains its accepted-peer policy independently;
outgoing checks are selected per connection. Policy is local configuration, not
negotiated with an untrusted peer and not part of the wire protocol.

## Strict defaults

* Immediate parent owned by the effective user with exact mode `0700`.
* Missing directories created with requested mode `0700` (subject to umask).
* Socket mode `0600`, applied after bind while the parent is private.
* Only root-owned ancestor symlinks allowed, not an immediate-parent symlink.
* Ancestors owned by root/effective user and not group/other writable, except
  root-owned sticky ancestors such as `/tmp`.
* Connected peers must have the effective user's UID on both sides.

## Explicit choices

`parent_owner` and `peer` independently accept the effective user, a specified
UID, or `Unrestricted`. `parent_mode` selects an exact mode or `None` to skip
mode validation. `directory_mode` and `socket_mode` select permission bits.
`create_parents: false` validates existing directories without creating them.
`symlinks` can reject all links, permit trusted system ancestors (default), or
follow links including the parent. `trusted_ancestors: false` skips ancestor
ownership/mode checks. All remain directory walks with descriptor-relative
operations; policy never grants permission to replace files or unlink endpoints.

For example, an application whose separate authentication scheme permits group
access could choose the following filesystem policy while retaining same-user
peer authentication until it deliberately chooses another peer policy:

```rust
use bmux_ipc::transport::UnixSecurityPolicy;
let policy = UnixSecurityPolicy {
    parent_mode: Some(0o750),
    directory_mode: 0o750,
    socket_mode: 0o660,
    ..UnixSecurityPolicy::strict()
};
```

Relaxations remove guarantees: shared directories expose the name before socket
chmod; following symlinks trusts redirection; unchecked ancestors can permit
endpoint substitution; disabling peer checks removes OS-user authentication.
The caller must supply whatever authorization its product requires. These APIs
are not authentication for remote/federated application operations.

The separate `UnixSecurityPolicy::prepare_private_directory` API supports
private-directory migration with strict or unchecked ancestor trust. It rejects
relaxed ownership, mode, and symlink policies before mutation. The free
`prepare_private_directory` helper retains strict ancestor checks.

Invalid mode bits fail before filesystem changes. Existing directories are never
chmod/chowned by socket binding or `prepare_parent`. Unavailable required peer
checks fail closed;
explicitly unrestricted peers skip the check. Non-Unix transports reject
non-default Unix policies instead of silently ignoring them. Unix peer
verification is currently supported on Linux and macOS.

`UnixSecurityPolicy::prepare_parent` exposes the same filesystem preparation and
validation for consumers with serialized stale-name recovery. It does not remove
sockets. Consumers own endpoint derivation, lifecycle serialization, cleanup,
and product-specific runtime paths. Library code does not adopt BMUX runtime
paths or environment configuration on behalf of consumers.
