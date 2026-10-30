# Application resurrection: implementation contract

Status: implemented for local Unix hosts. Bcode publishes session selection
through
an experimental host-independent control channel. The default-bundled,
disableable
BMUX plugin authenticates local peers, persists acknowledged descriptors, and
uses
v2 invocation identities to validate pane snapshot association. Shell markers
are
bound to each launch. Nested integrated shells use distinct command invocations
and a bounded checkpoint-v2 recovery path. Direct launch, Bash/Fish/Nushell
replay,
nested Bash/Zsh/Fish/Nushell, repeated restarts and manual-launch isolation have
real-daemon regression coverage. See the adapter README for startup integration;
BMUX never edits user startup files. The real Bcode TUI restart test passed with
a freshly built
Bcode
binary and isolated state; the replayed launch omitted the original session ID.

The cross-repository test is opt-in because it requires a Bcode binary. See
`plugins/application-resurrection-plugin/README.md` for its command, supported
scope,
and explicit recovery behavior. Unintegrated nested shells, non-exec wrappers
and remote applications are not automatically adopted. Same-user code with
access to private integration
files remains within the local trust boundary.

## Nested invocation recovery

The pane owner retains the outer command and invocation identity. The
application
resurrection plugin owns a bounded foreground command path beneath that root.
Each integrated shell reports its PID, executable and command boundaries through
short-lived callbacks using the ordinary BMUX executable. The host authenticates
the socket peer and verifies bounded native ancestry to the reporting shell;
client-supplied PIDs and inherited endpoints are not authority. Process births,
sessions, controlling terminals and foreground groups are checked. A bounded
job-control handoff grace period handles fork-before-`tcsetpgrp` races.

A shell command boundary atomically persists its new invocation before the
callback returns. Successive commands receive distinct identities and invalidate
old application registrations. One foreground child path is supported per
invocation; competing shell branches or application owners are rejected. The
entire nested descriptor path is committed atomically before application updates
are acknowledged. Checkpoint v2 is explicitly versioned; v1 remains readable.

On replay, each recreated shell registers, restores its saved cwd and signals
readiness. Terminal metadata carries only readiness, never executable commands.
The pane owner takes the host's pending command through the typed lifecycle
service and sends it to that shell. An offer is bound to a live shell
incarnation
and one invocation, and cannot be taken twice. Each recreated application
receives
only its own saved descriptor. A subsequent manual launch receives none.

This is command replay, not shell memory checkpointing. Shell-local environment,
functions and arbitrary wrapper side effects are not reconstructed. The existing
command replay policy applies. Bash and Zsh require startup integration for
ordinary nested launches; Fish and Nushell use inherited vendor autoload paths.
Callbacks add no separate executable artifact or long-running helper. Existing
pane execution supervision is unchanged by this integration.

## Product outcome

When a host resurrects a Bcode invocation, Bcode opens the persisted session
that
invocation was showing. Starting Bcode manually continues to use normal startup,
even in the same pane after a resurrected invocation exits. Existing composer
draft restoration is outside this change. Exact UI-state restoration is not a
goal.

The application-facing contract is host-independent. BMUX supplies the first
host
adapter as a dedicated, statically bundled, default-enabled, disableable plugin.
Bcode must not require BMUX to publish or consume application recovery metadata.

## Ownership

- Bcode owns the session reference, its authority, configuration, publication on
  session transitions, and reopening through the existing session-open API.
- The portable protocol defines discovery, registration, update, clear, release,
  version negotiation, and a restoration-only handoff. It does not define a
  command to execute, a shell fragment, or credentials to restore.
- The BMUX adapter owns protocol interpretation and application-resurrection
  policy. It consumes declared plugin services and events, not private runtime
  implementations.
- The pane-runtime plugin owns execution identity and lifecycle, launch context,
  shell replay, and the execution association of persisted opaque metadata.
- Snapshot orchestration remains with the existing snapshot plugin.
- Core, HostRuntimeApi, and shared schema generators acquire no application or
  pane-specific convenience operations.

Disabling the adapter preserves ordinary command resurrection. Static bundling
must use the existing activation and disablement mechanisms, not unconditional
runtime initialization.

## Configuration semantics

Proposed Bcode configuration:

```toml
[tui.session_resurrection]
mode = "auto" # auto | enabled | disabled
```

- `auto`: participate only after discovering a compatible host capability.
- `enabled`: enable participation using the configured portable transport even
  without automatic host discovery. Absence of a host is not permission to
  recover a global or directory-local "last session".
- `disabled`: neither publish descriptors nor consume restoration handoffs.

Explicit session selection takes precedence over a restoration descriptor. A
per-invocation opt-out is required. Configuration must follow Bcode's existing
resolution rules; transport discovery must not introduce another configuration
source of truth.

In every mode, automatic reopening requires an explicit, current restoration
handoff. BMUX presence, cwd, terminal title, PID, session recency, and a pane ID
alone are never sufficient evidence of restoration.

## Descriptor

A versioned envelope identifies the application and an application-owned
payload.
Bcode's payload identifies the session and its storage authority. The latter
must
use Bcode's actual resolved state/context authority, not a guessed cwd or a
transient daemon socket path. An endpoint address alone is not durable
authority.

The envelope must not include executable replacements, shell commands, arbitrary
environment assignments, private credentials, permission grants, or prompt text.
The host validates envelope version and resource bounds but does not interpret
Bcode's payload. Bcode validates its payload before opening the session through
normal application boundaries. Missing or incompatible state is reported without
opening an unrelated session or submitting work.

A clear operation is explicit. A missing update must not be interpreted as
clear,
and corrupt metadata must not become an apparently healthy empty descriptor.

## Invocation lifecycle

Registration is associated with a host-authoritative execution identity and
execution generation. A pane is a container, not an application invocation.
Private registration credentials are transient and never enter snapshots.

Required transitions:

1. Host allocates an execution identity and advertises supported protocol versions.
2. Application registers against that current execution.
3. A successfully selected persisted session produces an update with a revision
   precondition. Returning to an unpersisted draft produces clear.
4. Failed and stale asynchronous session-open completions cannot overwrite a newer
   selection. Reconnecting to the same selected session does not clear it.
5. Application release, execution exit, or a shell command boundary invalidates
   the registration and any unconsumed handoff. Transport disconnect by itself
   does not prove execution exit.
6. During resurrection the host advances execution generation and supplies the
   saved descriptor only to the recreated invocation.
7. A later manually entered command receives no usable restoration handoff.

Retries within one generation have a defined idempotent result. A stale process
cannot overwrite or consume state belonging to a replacement execution. Multiple
registrations, nested applications, wrappers, and pipelines require explicit
ownership rules; do not resolve ambiguity with last-writer-wins.

## Restoration delivery

An inherited environment value containing a reusable descriptor is insufficient:
children and a later manually launched command could reuse it. Discovery may use
an environment value, but consumption must be scoped to the current execution
and restoration attempt by the host.

Explicit program launches and interactive-shell replay are separate integration
paths. Shell replay needs a handoff active only for the replayed command, with
prompt/command lifecycle invalidation. The initial prompt that triggers replay
must not accidentally invalidate the pending restored command. A new prompt
following the replayed command must invalidate it.

Unsupported shells or ambiguous command ownership must produce an explicit
unsupported/degraded result rather than silently treating a pane as an
invocation.
Normal terminal operation and existing command resurrection remain available.

A host-side claim must not permanently discard the only recovery descriptor
before
an application has successfully reopened its session. Separate durable
descriptor
retention from generation-bound delivery/claim state and define interruption
behavior between claim, open, acknowledgement, and another host shutdown.

## Transport decision gate

Descriptor semantics are independent of transport. Before code is wired into
startup, select and document a transport that meets the lifecycle above.

Candidates:

- A bounded, version-negotiated local control channel with execution-scoped
  registration and restoration claims. Endpoint discovery alone is not authority;
  document how callers are associated with an execution on supported platforms.
- Terminal metadata advertisement with a separately scoped restoration handoff.
  Existing OSC user-variable carriers are possible carriers, not an existing
  resurrection standard. Arbitrary terminal output is untrusted, including output
  from children, SSH, printed logs, and replayed recordings.

Do not introduce an arbitrary OSC number, claim an existing interoperability
standard, or accept future command execution instructions from terminal output.
Any terminal transport must define bounded parsing, fragmented sequences,
forwarding/filtering, nested hosts, remote execution, capability discovery, and
whether/how updates are acknowledged. An acknowledgement claiming durability
must
follow the required storage commit, not just an in-memory update.

## Persistence and shutdown

BMUX currently seals its final snapshot before terminating its terminal
processes.
Therefore Bcode publishes during ordinary session transitions, not only at exit.

Descriptors must be captured coherently with the execution they describe. A
separate adapter snapshot cannot be restored after pane processes have already
started and still claim to provide startup handoff. Either use opaque execution
metadata in the pane owner's snapshot or establish explicit snapshot restore
ordering through declared contracts. Preserve existing snapshot data and reject
unsupported versions.

Keep I/O and persistence off the terminal frame loop. Bound descriptor sizes,
registration count, outstanding requests, and update work. Coalescing may retain
the newest unacknowledged update, but cannot acknowledge an update as durable
before committing it. Report persistence failure rather than claiming protection
against daemon loss.

## Implementation sequence

1. Resolve transport, caller/execution association, storage-authority representation,
   and shell-replay claim semantics. Write behavioral fixtures for the protocol.
2. Add narrow pane-runtime plugin contracts for the required execution lifecycle,
   opaque metadata, and scoped restoration delivery. Do not expose private runtime
   structs or add new HostRuntimeApi operations.
3. Implement the self-contained BMUX adapter, its snapshot participation as needed,
   and normal static bundling/disablement.
4. Implement Bcode configuration, startup consumption, and publication from its
   central session attachment lifecycle. Reuse existing session opening and draft
   restoration; never resubmit a prompt or repeat interrupted tool work.
5. Exercise Bcode with a non-BMUX test host, then end-to-end with BMUX for both
   explicit launches and supported shell replay.
6. Run each repository's required validation before claiming completion.

## Acceptance matrix

- Manual launch outside BMUX remains normal.
- Manual launch inside a compatible host remains normal.
- Resurrection reopens the invocation's selected persisted session.
- Session A to B transition restores B, not the initial CLI selection.
- A new draft clears the prior session reference.
- Failed/stale session opens cannot publish incorrect selection.
- Existing composer draft restoration still works, without new UI persistence.
- Two instances in one directory recover independently.
- Explicit selection and opt-out override restoration.
- A manual launch after restored Bcode exits cannot reuse its handoff.
- Nested/duplicate applications cannot consume or overwrite another registration.
- Explicit launch and supported shell replay obey the same protocol semantics.
- Adapter disablement preserves baseline BMUX resurrection.
- Other BMUX runtime slots and Bcode state authorities remain isolated.
- Graceful shutdown, abrupt daemon loss, failed claim, failed session open, and
  shutdown during claim acknowledgement have defined recovery outcomes.
- Unknown schemas, oversized payloads, missing sessions, and corrupt snapshots
  report explicit errors or degraded recovery states.
- A minimal non-BMUX host passes the same Bcode protocol tests.
- No recovery path bypasses authorization or repeats submitted work.

## Repository evidence (2026-09-19)

- `packages/pane-runtime-state/src/meta.rs`: explicit launch and shell-command
  resurrection data are distinct; neither currently has an application descriptor.
- `plugins/pane-runtime-plugin/src/runtime.rs`: shell metadata parser, prompt-based
  replay, direct program spawning, and snapshot-dirty marking.
- `plugins/pane-runtime-plugin/src/snapshot.rs`: pane-owner snapshot schema and
  restore dependency on the sessions snapshot.
- `plugins/snapshot-plugin/src/orchestrator.rs`: ordered plugin restore and final
  snapshot sealing.
- `packages/server/src/lib.rs`: final snapshot precedes runtime termination.
- `../bcode/packages/cli/src/lib.rs`: `bcode tui <session-id>` already opens the TUI;
  `attach` is not the interactive-TUI restoration operation.
- `../bcode/packages/tui/src/session_flow.rs`: central Draft/Opening/Attached/Detached
  view lifecycle and stale asynchronous open-result handling.

These are implementation entry points; production behavior is summarized at the
start of this document. They do not authorize extending domain behavior into
core infrastructure.
