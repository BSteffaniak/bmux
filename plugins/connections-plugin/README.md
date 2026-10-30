# bmux_connections_plugin

Bundled implementation of the transport-neutral `bmux.connections` domain.

It resolves local, SSH, TLS, and Iroh targets from bmux configuration and
invokes an opaque typed service request on one selected endpoint. Selection
policy remains with callers such as the cluster plugin; this crate does not
contain cluster, workspace, placement, or gateway policy.

## Endpoint and invocation semantics

- `resolved-endpoint.endpoint_id` is the canonical opaque endpoint identity accepted by `invoke-service`; transport details remain private to this plugin.
- Endpoint acquisition is pooled by fully resolved transport/security identity, with hard limits of 256 total and 64 per endpoint, deadline-aware wait backpressure, bounded idle reuse, and idle eviction across endpoints.
- TLS compression follows `behavior.compression.remote`; admitted device streams require the bounded uncompressed admission preamble and do not support explicit zstd.
- Successful one-shot service calls return healthy clients to the pool; failed or ambiguous calls discard them.
- A pooled `BmuxClient` can become a dedicated `StreamingBmuxClient` lease whose connection capacity remains charged for the stream lifetime.
- `invocation-options.timeout_ms` is one monotonic deadline covering resolution, every acquisition attempt, retry backoff, and service invocation. The default is 30 seconds.
- `max_attempts` defaults to one and is bounded to 10. Only `connection-failed` acquisition errors and acquisition timeouts are retried. Authentication, trust, target, and service errors are never retried.
- Service invocation is never replayed after dispatch because completion may be ambiguous and commands may be non-idempotent.
- `retry_backoff_ms` is fixed between acquisition attempts and bounded to 30 seconds.
- Host cancellation is checked before resolution, before each acquisition attempt, and between retries. Cancellation, timeout phase, and retry exhaustion are structured errors.

## Paired device endpoints

`device://<endpoint-public-key>?addr=<socket-address>` pins the remote host key.
A relay address can instead be supplied as `?relay=<relay-url>`.
`listener-status.endpoint_targets` contains public, source-independent endpoint
URIs; never advertise a host-issued, source-specific `device-id` in cluster
metadata.
The `iroh://` spelling remains supported, but dialing also requires owner
pairing.

Use generated BPDL clients over each runtime's trusted local `BmuxClient`:

1. Query the connecting runtime with `pairing_state::client::local_device_identity`.
2. On the destination, call `pairing_commands::client::create_invitation` with
   that public device key, a bounded TTL, and the intended device access grant.
3. On the connecting runtime, call `pairing_commands::client::pair_endpoint`
   with the destination's public URI and the invitation. It verifies the host
   and device keys, redeems proof of possession, and persists a private mapping
   from the pinned host key to this device's issued subject. The returned URI
   is the same public URI, not an authority-bearing credential.
4. Invoke generated service payloads through `connections_commands::client::invoke_service`.

Each direction is paired separately. Outbound mappings and keys survive runtime
restart in runtime-namespaced private plugin storage. An explicit `device-id`
option, if supplied for compatibility, must match that local binding; it cannot
create trust. Re-pair locally to replace a binding after revocation. Invitations
and their secrets are never stored in the outbound map or endpoint URI.

For an isolated direct listener with a stable restart address, set these keys in
the connections plugin's settings (not core transport configuration):

```toml
[remote]
allow_full_runtime_ingress = true
bind_addr = "127.0.0.1:40201"
auto_start = true

[remote.relay]
mode = "disabled"
```

`auto_start` requires explicit ingress permission and a nonzero fixed UDP port.
If binding that listener fails (for example, another server owns the port),
local
startup continues without remote ingress. Listener status retains a warning,
shown on local attach and by `bmux remote status`. After resolving the conflict,
retry with `bmux remote enable --yes`. Explicit retries still report failures;
BMUX never switches ports or stops the process occupying the configured address.
Configuration, identity, and admission failures remain fatal rather than
silently
disabling security checks.
Without auto-start, a local owner uses `start-listener`; that command also
checks
same-instance backend admission registration. Configuration-enabled auto-start
may run before backend readiness, but every stream still requires backend ticket
consumption, subject-bound Hello, and revocation checks: there is no unadmitted
fallback. Bind failures fail activation rather than selecting a different port.

Pairing grants access to the local runtime only, never membership. Destination
plugins declare selected admitted service routes via
`bmux.stream-admitted-routes.v1` and authorize their own operations. Connections
contains no cluster route special cases and does not restore legacy TLS ingress.
