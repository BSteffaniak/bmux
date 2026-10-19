# Bterm

Minimal native BMUX window. BMUX plugins still own commands, tabs, splits,
workspaces, execution and permissions; the window owns only native lifecycle
and graphics resources. The existing retained presentation is rendered directly,
without ANSI capture, a browser listener, or a second layout/hit-test model.

`--runtime-root /absolute/root` selects an external runtime, attach-only.
It conflicts with managed companion options. Managed selection uses a verified
BMUX companion and a complete persistent root, never a runtime-name fallback.
Window closure detaches; it does not stop the companion or its executions.
Failures are printed to stderr and return a failing process status.

Managed startup re-verifies the companion bundle, then serializes endpoint
probing and readiness under a persistent ownership-file lock. Only definitive
absence permits launch; incompatible peers and uncertain transport or
authentication failures never authorize replacement. The detached companion
receives complete explicit roots and a deliberately rebuilt environment, and
outlives the window. External selection never acquires launch authority.

`app::Application` runs on the native main thread. `app::AttachWorker` connects
that host to the canonical BMUX orchestration worker. `window::NativeOutput`
provides a single-slot synchronous presentation handoff: enqueueing is not
success. The observer receives success only after actual renderer presentation.
Closing/dropping the receiver rejects blocked publication. Pixel metrics project
presentation cells independently of execution geometry.

## Integration

The native frontend currently hosts the existing `bmux_cli::attach` orchestrator
through its no-`Write` native host. Runtime state and enabled plugin adapters are
initialized on the attachment worker thread with the selected root's explicit
configuration paths, including lazy command dispatch. This is deliberate reuse,
not a second plugin manager. The window never serializes a browser frame.

Linux and macOS use the native platform backend. Unsupported backend and GPU
initialization failures are errors, not successful empty windows. Runtime
selection/compatibility and startup are separate modules so that window lifetime
cannot accidentally acquire runtime termination authority.
