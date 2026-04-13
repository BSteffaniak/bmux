# bmux Android app (planned)

Android client for connection-first bmux remote workflows.

M4 status: baseline Compose Android app module is now scaffolded with target
import/list, connect, and host-key observe/pin flows wired to UniFFI.

M5 status: discovery, reconnect, and security hardening are now wired:

- mDNS LAN discovery using Android NSD
- reconnect foreground service with exponential backoff loop
- encrypted local persistence for targets, pin history, discovery history,
  and target health timestamps
- runtime permission prompts for discovery and notifications on Android 13+
- androidTest coverage for reconnect/backoff behavior and secure-store migration

## UniFFI Kotlin Binding Task

This folder includes a tiny Gradle task for generating Kotlin bindings from
`bmux_mobile_ffi`.

From `apps/android`:

1. `./gradlew generateUniffiKotlinBindings`

The task will:

- run `cargo build -p bmux_mobile_ffi`
- run `cargo install --locked --root apps/android/.tools uniffi --version 0.31.0 --features cli` (if missing)
- run `.tools/bin/uniffi-bindgen generate --language kotlin`
- write output under `apps/android/app/src/main/java/uniffi`

## Run M4 app shell

From `apps/android`:

1. `./gradlew :app:assembleDebug`
2. `./gradlew :app:assembleDebugAndroidTest`

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
