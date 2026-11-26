# bmux Android app (planned)

Android client for connection-first bmux remote workflows.

Paired-host status: the app is a paired bmux device, like `bmux connect` on a
desktop.

- Pairing: paste the `bmux-pair:` link from `bmux remote pair` on the host
  (hidden input), compare the phrase both devices show, and approve on the
  host. The host is saved with its pinned host key and granted role.
- Paired host list with connect, terminal, reconnect and forget actions.
- Device key and saved hosts live in the Rust core under
  `noBackupFilesDir/bmux`; encrypted preferences keep only the last host and
  per-host connection health.
- Reconnect foreground service retries `connect` with exponential backoff.
- Old saved targets (`iroh://`, `ssh://`, `host:port`) are not migrated; pair
  again. LAN discovery and SSH host-key pinning were removed with them.

Earlier milestones: M6 added the `alpha` build type (`io.bmux.android.alpha`),
`packageInternalAlpha`, alpha telemetry in logcat (`BmuxAlpha`) and the internal
test checklist; M7 started the terminal renderer (`org.connectbot:termlib`).

## UniFFI Kotlin Binding Task

This folder includes a tiny Gradle task for generating Kotlin bindings from
`bmux_mobile_ffi`.

From `apps/android`:

1. `./gradlew generateUniffiKotlinBindings`
2. `./gradlew packageInternalAlpha`
3. `./gradlew collectAlphaLogs`

The task will:

- run `cargo build -p bmux_mobile_ffi`
- run `cargo install --locked --root apps/android/.tools uniffi --version 0.31.0 --features cli` (if missing)
- run `.tools/bin/uniffi-bindgen generate --language kotlin`
- write output under `apps/android/app/src/main/java/uniffi`

## Run M4 app shell

From `apps/android`:

1. `./gradlew :app:assembleDebug`
2. `./gradlew :app:assembleDebugAndroidTest`
3. `./gradlew :app:connectedDebugAndroidTest`

Main UI file:

- `app/src/main/java/io/bmux/android/MainActivity.kt`

Product direction for v1:

- pair once, then connect to saved hosts from anywhere (direct or relay)
- session selection and reconnect workflows
- background reconnect via Android foreground service

See:

- `docs/mobile-m0-architecture.md`
- `docs/mobile-ffi-contract.md`
- `apps/android/docs/internal-alpha-checklist.md`

`collectAlphaLogs` writes filtered alpha telemetry logs to:

- `apps/android/logs/alpha/bmux-alpha-<timestamp>.log`
