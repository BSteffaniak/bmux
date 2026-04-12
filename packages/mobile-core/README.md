# mobile-core (planned)

Shared Rust domain core for bmux mobile clients.

This crate is planned in M1. M0 only defines architecture and API contracts.

Expected responsibilities:

- target parsing and canonicalization
- transport-agnostic connection state machine
- iroh/tls/ssh transport adapters
- session metadata APIs for connection-first mobile flows
- typed diagnostic and error model for platform UIs

See:

- `docs/mobile-m0-architecture.md`
- `docs/mobile-ffi-contract.md`
