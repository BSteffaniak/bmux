# bmux_connections_plugin

Bundled implementation of the transport-neutral `bmux.connections` domain.

It resolves local, SSH, TLS, and Iroh targets from bmux configuration and invokes an opaque typed service request on one selected endpoint. Selection policy remains with callers such as the cluster plugin; this crate does not contain cluster, workspace, placement, or gateway policy.

## Endpoint and invocation semantics

- `resolved-endpoint.endpoint_id` is the canonical opaque endpoint identity accepted by `invoke-service`; transport details remain private to this plugin.
- Endpoint acquisition is pooled by fully resolved transport/security identity, with hard limits of 256 total and 64 per endpoint, deadline-aware wait backpressure, bounded idle reuse, and idle eviction across endpoints.
- TLS and Iroh endpoint clients apply the same configured `behavior.compression.remote` policy as their server gateways; local and SSH transports are not wrapped by this layer.
- Successful one-shot service calls return healthy clients to the pool; failed or ambiguous calls discard them.
- A pooled `BmuxClient` can become a dedicated `StreamingBmuxClient` lease whose connection capacity remains charged for the stream lifetime.
- `invocation-options.timeout_ms` is one monotonic deadline covering resolution, every acquisition attempt, retry backoff, and service invocation. The default is 30 seconds.
- `max_attempts` defaults to one and is bounded to 10. Only `connection-failed` acquisition errors and acquisition timeouts are retried. Authentication, trust, target, and service errors are never retried.
- Service invocation is never replayed after dispatch because completion may be ambiguous and commands may be non-idempotent.
- `retry_backoff_ms` is fixed between acquisition attempts and bounded to 30 seconds.
- Host cancellation is checked before resolution, before each acquisition attempt, and between retries. Cancellation, timeout phase, and retry exhaustion are structured errors.
