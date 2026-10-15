# Managed playbook execution

`bmux.playbook` is an independently hostable native service plugin. It owns up to
16 retained attempts, each with a random generation and separate private roots.
The typed `playbook-state` and `playbook-commands` interfaces are generated from
`playbook-plugin-api/bpdl/playbook-plugin.bpdl`. No presenter registration is needed.

## Explicit application environment

Managed workers still clear the host environment. The owner may explicitly opt
in application inputs using `BMUX_PLAYBOOK_APP_ENV_<NAME>=<value>` in its own
launch environment. Preparation captures a bounded set in memory; only the real
runner receives `<NAME>=<value>` (not the compatibility probe). Values are not
part of preparation JSON, frozen script sources, or status/evidence snapshots.
Runtime/configuration selectors remain protected. Invalid names or more than
32 entries / 64 KiB of values reject preparation with a value-free error.
This is not a secret store or an application sandbox: approved applications and
their inherited child processes can read these inputs. Do not share an owner
between untrusted applications. A fresh owner is required to change its launch
environment; no late environment lookup or secret-file substitution is implied.

## Execution boundary

The plugin invokes an explicitly selected native BMUX executable's existing
`playbook run` action executor. It first probes worker protocol
`bmux-playbook-worker-v1` using a nonexistent input (an old executable cannot
accidentally execute the real deck). The runner uses private atomic retained
control/snapshot files, not stdout parsing. stdout/stderr are not display feeds.
The same parser/model implementation lives in the `bmux_playbook` domain library
and is consumed directly by CLI and this plugin.
The action executor remains in the selected BMUX worker, with CLI signal/terminal
acquisition bypassed in managed mode. This is a production process boundary, not
an optional injected/mock driver.

Preparation requires `request_id` (UUID) and `expected_closure_sha256`, computed by
`bmux_playbook::parser::source_closure_sha256` when approving the full include
closure. `freeze_sources_verified` checks the approved hash and writes the exact
same bounded in-memory names/bytes, without reopening originals. Changed includes
are rejected. Hash framing includes the entry name and sorted normalized relative
names and bytes under the `bmux-playbook-closure-v1` domain separator.

Within the current owner lifetime, repeated prepare with identical request ID and
fields returns the original attempt without rereading source or creating effects.
Conflicting reuse is rejected. `playbook-state.lookup-preparation` recovers the
retained attempt after an uncertain reply; missing identity is an explicit error,
not proof that an earlier host lifetime never admitted it. Callers must not turn
that error into successful settlement or automatically rerun unknown actions.

Start returns retained `probing` state promptly. The 25 ms monitor owns the probe
child, checks its completion with `try_wait`, enforces its deadline and launches
the runner only after compatible success. Cancellation kills the probe but must
observe/reap exit before acknowledgement. No probe wait/sleep occurs under the
owner mutex.

Preparation requires absolute source, allowed source root, executable and existing
attempts root, and explicit executable-input trust. Host capability enforcement
for `bmux.playbook.execute` is still required; the trust boolean grants no remote
permission. Generations isolate attempts but are not principal authentication.
Source/include closure is frozen per attempt: no symlinks, directory-relative
opened reads, at most 128 files / 4 MiB total / 1 MiB each / existing include depth
limit. Restart copies the frozen source, not the original mutable deck. Custom
sandbox configuration and runtime-selector environment overrides are rejected.
This is not a security sandbox: trusted actions may intentionally execute arbitrary
shell commands and access other files.

## Presenter integration

Call generated `playbook_commands::client::prepare` with `Preparation`, then
`start(generation)`. Poll generated `playbook_state::client::watch` with generation
and last revision. Each response is a complete retained `Attempt` snapshot.
`phase`, `step`, `session_id`, explicit config/runtime/data/state directories and
cleanup state are authoritative for that generation. There is no terminal data in
this lifecycle service. Construct normal BMUX `ConfigPaths` from the four returned
directories and connect/attach to `session_id` through the existing committed BMUX
display/attachment protocol. Terminal and browser presenters must use the same
retained compositor, not screen text, diagnostic JSON or ANSI runner output.

Pause applies at action boundaries; a step permit executes exactly one entire
action, including waits and side effects. No seek is provided. Stop is sticky and
interrupts asynchronous action waits; ordinary action deadlines also apply while
paused. Successful script completion keeps the real sandbox alive for viewing
until explicit stop. Managed execution uses the real sandbox driver; CLI-only
real-attach/simulation drivers are deliberately rejected.

Managed sandbox startup owns a foreground child using the selected worker binary;
there is no daemon or ordinary user-runtime fallback. A cleanup acknowledgement
requires sandbox child reaping and runner exit. Emergency runner termination
NEVER grants acknowledgement, removes diagnostics or enables restart. Failed
attempts retain roots. The service background monitor progresses child reaping and
stop deadlines even when the presenter stops polling.

`examples/pane-split.playbook` is the simple real two-pane example. It has not
been executed by this source-only implementation lane; integration/validation is
owned by the coordinator.

## Known integration constraints

* Central workspace/CLI dependencies and bundled plugin registration are owned by
  the coordinator. CLI consumes `bmux_playbook` directly; linking this execution
  plugin remains optional and is not required for parser/model availability.
* This implementation does not yet persist active ownership across host crashes,
  reclaim retained attempt capacity, or bind individual generations to remote
  authenticated principals. Do not expose it as a multi-tenant remote execution
  service without that owner authorization policy.
* The protocol probe establishes worker compatibility, not an executable-content
  hash. Operators must supply an immutable selected artifact path.
* No new display-feed API is introduced; presenter integration remains required
  and is not proven by the lifecycle service alone.
