# bmux Android Internal Alpha Checklist

## Build And Install

1. From `apps/android`, run `./gradlew packageInternalAlpha`.
2. Install with `./gradlew :app:installAlpha`.
3. Confirm app id `io.bmux.android.alpha` launches on device.

## Dogfood Validation

- On the host, run `bmux remote enable`, then `bmux remote pair`.
- Paste the pairing link into the app, compare the phrase on both devices, and
  approve on the host. The host appears under Paired hosts with its role.
- Connect to the host and confirm the status updates.
- Open a terminal, type, and confirm the host's own view shows the input.
- Revoke the device on the host (`bmux remote revoke`) and confirm Connect now
  reports that the host refuses this device.
- Enable reconnect service and verify foreground notification appears.
- Force close app, reopen, verify paired hosts persist.

## Failure Capture

Capture logs after reproducing an issue:

- `adb logcat -d | grep BmuxAlpha`
- `adb logcat -d | grep -i "bmux\|uniffi\|ConnectionForegroundService"`

Capture app/build info:

- Gradle task output used (`packageInternalAlpha`, `connectedDebugAndroidTest`)
- Device model and Android version

## Known Alpha Limitations

- Reconnect behavior is foreground-service based and not optimized for battery.
