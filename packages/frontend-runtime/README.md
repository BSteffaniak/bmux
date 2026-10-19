# Frontend runtime

Shared provider attachment state and native presentation admission. Providers
retain execution, authentication, recovery policy, and typed plugin ownership;
this crate creates no resources, windows, daemon, listener, or runtime root.

`NativeAttachDriver` validates neutral provider snapshots/deltas before exposing
existing terminal parser buffers. Disconnect fences controls. Reconnect accepts
an explicitly authenticated replacement and complete repair; hosts must also
rebind presentation authority. Detach is explicit and idempotent.

`PresentationAuthority` accepts only the current epoch, target, viewport and
latest staged revision. Only successful synchronous complete backend output
advances its committed prepared frame. Failed/stale output cannot advance
interaction metadata. It is an acceptance gate, not a second compositor/hit map.

`input` owns backend-neutral logical/physical keys, phase, committed text, IME,
and physical pointer/scroll contracts, not scenes, hit maps or PTY execution.
Preedit never becomes execution input; IME text is not bracketed paste.

The initial local BMUX integration temporarily hosts its existing orchestrator
in `bmux_cli::attach::run_native_with_client`, with explicit optional terminal
output. Native output uses the same retained compositor and direct
`PreparedDelivery` acceptance, without ANSI encoding, fake writers or browser
serialization. The CLI remains independently usable. Native hosts initialize
client plugins on their attach worker using explicit selected config paths;
no native-to-CLI dependency exists in this crate.
