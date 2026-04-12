# mobile-ffi

UniFFI bridge crate for exposing `mobile-core` APIs to Android and iOS.

Current status:

- M3 base API is implemented with UniFFI-compatible exported objects and records.
- The FFI surface includes target import/listing, connection state transitions, and SSH host-key pinning helpers.

Primary exported object:

- `MobileApiFfi`

Binding generation (example workflow):

1. Build the library:
   - `cargo build -p bmux_mobile_ffi`
2. Generate Kotlin bindings:
   - `uniffi-bindgen generate --library target/debug/libbmux_mobile_ffi.dylib --language kotlin --out-dir apps/android/generated/uniffi`
3. Generate Swift bindings:
   - `uniffi-bindgen generate --library target/debug/libbmux_mobile_ffi.dylib --language swift --out-dir apps/ios/generated/uniffi`

Notes:

- Depending on platform and profile, the library file extension/path may differ.
- Treat generated files as build artifacts and keep Rust exports as the source of truth.

See:

- `docs/mobile-m0-architecture.md`
- `docs/mobile-ffi-contract.md`
- `docs/mobile-adr-0001-ui-and-shared-core.md`
