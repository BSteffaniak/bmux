# mobile-core

Shared Rust domain core for bmux mobile clients.

This crate provides shared Rust domain primitives for bmux mobile clients.

Current responsibilities:

- target parsing and canonicalization
- transport-agnostic connection state machine
- iroh/tls/ssh transport adapters
- session metadata APIs for connection-first mobile flows
- terminal stream session APIs (`open/poll/write/resize/close`)
- typed diagnostic and error model for platform UIs

See:

- `docs/mobile-m0-architecture.md`
- `docs/mobile-ffi-contract.md`
