# bmux Android Internal Alpha Checklist

## Build And Install

1. From `apps/android`, run `./gradlew packageInternalAlpha`.
2. Install with `./gradlew :app:installAlpha`.
3. Confirm app id `io.bmux.android.alpha` launches on device.

## Dogfood Validation

- Add target by manual URI (`iroh://`, `ssh://`, `host:port`).
- Start LAN discovery and import at least one discovered target.
- Connect to one target and confirm status updates.
- Observe and apply SSH host key pin on one SSH target.
- Enable reconnect service and verify foreground notification appears.
- Force close app, reopen, verify targets and recents persist.

## Failure Capture

Capture logs after reproducing an issue:

- `adb logcat -d | grep BmuxAlpha`
- `adb logcat -d | grep -i "bmux\|uniffi\|ConnectionForegroundService"`

Capture app/build info:

- Gradle task output used (`packageInternalAlpha`, `connectedDebugAndroidTest`)
- Device model and Android version

## Known Alpha Limitations

- No terminal rendering UI yet (connection manager only).
- Discovery is local network best effort and may vary by network policy.
- Reconnect behavior is foreground-service based and not optimized for battery.
