# BMUX Plugin Definition Language (BPDL)

BPDL is the typed interface description language for BMUX plugins. It lets
plugin authors declare the shapes (records, variants, enums) and operations
(queries, commands, request-scoped streaming commands, events) that define a
plugin's public contract, and provides codegen to produce idiomatic Rust
bindings for consumers.

## Contract policy

New typed plugin contracts must be expressed in BPDL and consumed through
BPDL-generated bindings. Plugin API crates are stable contracts, not transport
implementation crates:

- do not add public handwritten `typed_client.rs` modules;
- do not add broad public request/response envelope types for services already
  modeled in BPDL;
- do not map request-scoped streaming operations onto global/interface event
  streams. Interface `events` remain ambient pub/sub or state-channel events;
- use request-scoped streaming operations for workflows with one typed request,
  zero or more typed event frames, and exactly one typed final response.

The transitional streaming syntax is:

```bpdl
command start-turn(StartTurnRequest) -> FinishTurnResponse emits ProviderTurnEvent;
```

Generated bindings for streaming operations must preserve the request scope,
carry cancellation via the invocation id/control frame, and document bounded
backpressure behavior for emitted frames.

See `docs/bpdl-spec.md` (workspace root) for the full grammar and semantics.
