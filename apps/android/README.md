# bmux Android app (planned)

Android client for connection-first bmux remote workflows.

M4 status: baseline Compose Android app module is now scaffolded with target
import/list, connect, and host-key observe/pin flows wired to UniFFI.

## UniFFI Kotlin Binding Task

This folder includes a tiny Gradle task for generating Kotlin bindings from
`bmux_mobile_ffi`.

From `apps/android`:

1. `./gradlew generateUniffiKotlinBindings`

The task will:

- run `cargo build -p bmux_mobile_ffi`
- run `cargo install --locked --root apps/android/.tools uniffi --version 0.31.0 --features cli` (if missing)
- run `.tools/bin/uniffi-bindgen generate --language kotlin`
- write output to `apps/android/generated/uniffi`

## Run M4 app shell

From `apps/android`:

1. `./gradlew :app:assembleDebug`

Main UI file:

- `app/src/main/java/io/bmux/android/MainActivity.kt`

Product direction for v1:

- quick target import and connection
- iroh, SSH, and LAN TLS connectivity
- session selection and reconnect workflows
- mDNS discovery support
- strict TLS verification with optional pinning
- background reconnect via Android foreground service

Terminal emulation is out of scope for v1, but architecture should not block it.

See:

- `docs/mobile-m0-architecture.md`
- `docs/mobile-ffi-contract.md`
