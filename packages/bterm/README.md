# Bterm

Minimal native BMUX window. BMUX plugins still own commands, tabs, splits,
workspaces, execution and permissions; the window owns only native lifecycle
and graphics resources. The existing retained presentation is rendered directly,
without ANSI capture, a browser listener, or a second layout/hit-test model.

`--runtime-root /absolute/root` selects an external runtime, attach-only.
It conflicts with managed companion options. Managed selection uses a verified
BMUX companion and a complete persistent root, never a runtime-name fallback.
On Linux and macOS, managed transports live in `run/<kernel-boot-uuid>/` under
that root; configuration, data, and durable state stay in `config/`, `data/`,
and `state/`. All windows in the same boot reuse the same endpoint. A reboot
selects a fresh transport namespace without trusting persisted device numbers
or deleting old sockets/locks. External `--runtime-root` paths retain the
explicit `runtime/` layout and are never migrated or repaired by Bterm.

When upgrading from the legacy managed `runtime/` layout, Bterm checks and
locks its existing lifetime claim during startup. A running/starting legacy
companion, live endpoint, or uncertain ownership blocks migration; explicitly
stop that selected companion first. Stopped legacy files are preserved, even
when their device-number receipt no longer matches after reboot. Old boot
namespaces are not automatically deleted.

Window closure detaches; it does not stop the companion or its executions.
Failures are printed to stderr and return a failing process status.

Readiness is checked through the pane-runtime plugin before attachment. Managed
recovery is bounded to one graceful restart, only for an exact owned unhealthy
activation whose execution authority accepts a safety fence. Active or retained
executions, unknown ownership, and legacy daemons without the readiness contract
are preserved with a diagnostic. See [reconciliation](../../docs/bterm-reconciliation.md).

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

## Package ownership

The application package and executable are `bterm`. Bterm-owned native backends
are `bterm_native_platform` and `bterm_native_renderer`; new Bterm-specific crates
use the `bterm_*` prefix. Shared attachment, presentation, fonts, protocols, and
plugin contracts retain their existing BMUX names. Inclusion in Bterm's dependency
tree does not make a crate Bterm-specific.

Build the frontend with `cargo build -p bterm --bin bterm`, or build its verified
companion bundle with `cargo xtask bterm build`. Package naming does not change
runtime roots, executable names, or attachment and recovery contracts.

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
