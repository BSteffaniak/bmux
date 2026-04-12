# mobile-ffi (planned)

UniFFI bridge crate for exposing `mobile-core` APIs to Android and iOS.

This crate is planned in M1. M0 only defines architecture and API contracts.

Expected responsibilities:

- stable FFI API surface
- generated Kotlin and Swift bindings
- event stream bridging for connection/discovery updates
- compatibility/versioning discipline for app releases

See:

- `docs/mobile-m0-architecture.md`
- `docs/mobile-ffi-contract.md`
- `docs/mobile-adr-0001-ui-and-shared-core.md`
