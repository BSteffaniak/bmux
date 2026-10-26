# mobile-ffi

UniFFI bridge crate for exposing `mobile-core` APIs to Android and iOS.

Current status:

- M3 base API is implemented with UniFFI-compatible exported objects and records.
- The FFI surface includes target import/listing, connection state transitions, SSH host-key pinning helpers, and terminal stream APIs.

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

## Native pointer contract

`poll_terminal_presentation(terminal_id, max_chunks, version)` supports version
1
only and rejects unknown versions before consuming output. Its chunks retain the
existing byte contract; optional pointer metadata applies after those chunks.
`TerminalPointerShapeFfi` distinguishes `Hidden` from `Shape { name }`
(validated
CSS name), with attachment generation and monotonic revision. No raw OSC 22 is
sent to native consumers. Existing `poll_terminal_output` remains byte-only.

Android's gateway explicitly requests v1. The termlib adapter queues emulator
byte
acceptance and native pointer updates in FIFO main-thread order. Hover/leave are
structured input events; native icon mapping and failure/disposal reset are
local
output adaptation, not a second scene resolver. Generated bindings must be
rebuilt
from these Rust exports before compiling the Android application.

See:

- `docs/mobile-m0-architecture.md`
- `docs/mobile-ffi-contract.md`
- `docs/mobile-adr-0001-ui-and-shared-core.md`
