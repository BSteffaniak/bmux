# Bterm build and runtime reconciliation

`cargo xtask bterm build` produces a verified, content-addressed bundle. It never
starts or probes a runtime. Generations include the frontend, companion, helper
inventory and helper bytes, font notices, and launcher metadata. Publication
uses a private same-filesystem staging directory and an atomic rename. Identical
inputs reuse a verified generation; corrupt generations fail closed. A separate
atomic `latest.json` reference is informational, never execution authority.
Published generations and legacy bundles are retained: a running process must
not lose the executable or helper paths from its activation. Garbage collection
is intentionally not implicit.

The managed runtime root remains checkout-scoped and independent of build
identity. Rebuilding does not redirect a user into an empty runtime, migrate
state, or stop an existing execution. Window closure still only detaches.

## Observation and policy

Bterm's pure reconciliation policy maps explicit observations to start, wait,
attach, fence-and-restart, or refusal. Transport authentication, protocol errors,
unsupported readiness contracts, and unknown ownership never authorize process
replacement. Startup remains bounded and uses the existing lifetime ownership
claim. A runtime-root reconciliation lock serializes managed reconcilers across
build directories; each effect is followed by a new observation.

The pane-runtime plugin owns `pane-runtime-readiness-v1`. Activation captures an
incarnation UUID, PID, executable checksum, explicit roots, and the actual
auxiliary-resolution error or protocol mismatch. An IPC handshake alone does
not establish execution readiness. The response includes a conservative restart
blocker. Live session runtimes, retained reservation files, supervisor locators,
and unreadable authority directories all prevent automatic restart.

A managed activation receipt records only the exact instance that Bterm started
and verified against its selected companion checksum and returned PID. The
receipt is a prerequisite, not execution authority. Recovery additionally
requires the authenticated server-control principal, matching roots, and a
server-side exact-instance fence. An unhealthy activation cannot admit processes
or new reservation records; the fence prevents admission even after recovery is
requested. The frontend verifies the fence on the same connection before asking
for graceful shutdown, waits for authoritative absence, then starts the selected
bundle. Only one recovery attempt is allowed per invocation. A healthy activation
is never automatically restarted merely because a newer build exists.

## Conservative migration

A daemon started before this readiness contract or before an activation receipt
exists cannot prove automatic recovery safety. It is preserved with an explicit
error. There is no safe way to infer absence of executions from a failed window,
a missing helper on disk, a PID file, or an empty presentation. Such legacy
instances require an explicit user-directed stop once; later managed instances
participate in the reconciliation protocol. Nothing deletes their stored state.

Runtime/plugin implementation remains in the pane-runtime plugin. The API crate
contains only the generated BPDL wire contract. Core host APIs are unchanged.
