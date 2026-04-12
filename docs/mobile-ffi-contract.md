# Mobile FFI Contract (M0 Draft)

This document defines the initial API contract between platform apps and shared Rust.

Status: draft for M0 alignment. Final names and signatures can evolve in M1 before freeze.

## Design Principles

- Keep platform-facing APIs stable and coarse-grained.
- Keep platform-neutral types in FFI (no Android/iOS concepts).
- Return structured errors with machine-readable codes and user-safe messages.
- Stream connection/discovery state via events, not polling-only APIs.

## Proposed Crates

- `packages/mobile-core`: internal domain logic
- `packages/mobile-ffi`: UniFFI exports

## Target Types

`TargetInput` (import request):

- source string (URI/name/host entry)
- optional display name override
- optional transport hints

`TargetRecord` (persisted):

- `id` (stable UUID)
- `name` (user label)
- `canonical_target` (normalized target reference)
- `transport` (`local`, `ssh`, `tls`, `iroh`)
- `default_session` (optional)
- `security` (TLS pin info, strict host key settings)
- `metadata` (extensible map for future cluster/gateway info)

## Core API Surface (Draft)

`Target APIs`

- `import_target(input: TargetInput) -> TargetRecord`
- `list_targets() -> Vec<TargetRecord>`
- `update_target(record: TargetRecord) -> TargetRecord`
- `delete_target(target_id: String) -> ()`
- `test_target(target_id: String, timeout_ms: u64) -> TestResult`

`Connection APIs`

- `connect(request: ConnectRequest) -> ConnectionHandle`
- `disconnect(handle_id: String) -> ()`
- `reconnect(handle_id: String) -> ()`
- `connection_status(handle_id: String) -> ConnectionStatus`

`Session APIs`

- `list_sessions(handle_id: String) -> Vec<SessionSummary>`
- `select_session(handle_id: String, selector: SessionSelector) -> SessionSelection`

`Discovery APIs`

- `start_discovery() -> DiscoveryHandle`
- `stop_discovery(handle_id: String) -> ()`
- `discovery_events(handle_id: String) -> Stream<DiscoveryEvent>`

`Security APIs`

- `set_tls_pinning(target_id: String, pin: TlsPinPolicy) -> ()`
- `clear_tls_pinning(target_id: String) -> ()`
- `ssh_known_host_decision(input: SshHostDecisionInput) -> SshHostDecision`

`Event Streams`

- `connection_events(handle_id: String) -> Stream<ConnectionEvent>`
- `diagnostic_events() -> Stream<DiagnosticEvent>`

## Event Models (Draft)

`ConnectionEvent`:

- `connecting`
- `connected`
- `reconnecting`
- `auth_required`
- `degraded`
- `disconnected`
- `failed`

`DiscoveryEvent`:

- `candidate_found` (host/address/transport/metadata)
- `candidate_updated`
- `candidate_lost`
- `scan_error`

## Error Model (Draft)

All public calls return `Result<T, MobileError>`.

`MobileError` fields:

- `code`: stable enum (`invalid_target`, `tls_verify_failed`, `ssh_auth_failed`, `timeout`, `unreachable`, `protocol_incompatible`, `internal`)
- `message`: user-safe summary
- `details`: optional structured debug payload
- `retryable`: bool

## Persistence Boundary

FFI contract should not hardcode storage backends.

- Rust core defines repository traits for targets/recents/trust records.
- Platform layer supplies encrypted persistence adapters.
- Android implementation target: Keystore-backed encrypted storage.

## Backward Compatibility Rules

- Additive fields only for minor updates.
- Do not repurpose enum variants.
- New transport capabilities must be feature-detectable.
- Keep deprecated APIs available for at least one minor app release.

## Open Items for M1

- Final SSH config shape for mobile (jump hosts, identity material strategy).
- Exact discovery payload fields for mDNS candidate confidence and dedupe.
- Threading model for stream callbacks on Kotlin/Swift runtimes.
- Which APIs are sync vs async in UniFFI boundary.
