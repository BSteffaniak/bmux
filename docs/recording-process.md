# Recorded processes

## Record an interactive program

Run a program normally in an undecorated terminal, then finalize its recording
when it exits:

```sh
bmux record -- lazygit
bmux record --output ./capture --export raw --export text --export events -- nvim
bmux record --gif ./demo.gif -- nvim
```

Arguments after `--` are literal argv, not shell text. The command resolves the
program through PATH and uses the current directory, environment, terminal size,
and full capture profile. Terminal stdin and stdout are required; use
`bmux recording process` for headless interaction instead. Output directories and
GIF destinations must be new: existing evidence is not overwritten. GIF export
is optional, not part of ordinary recording startup.

Keyboard, paste, mouse, and resize input target only the recorded execution.
BMUX navigation shortcuts and decorations are absent. The server owns terminal
parsing and query replies; application escape sequences are not blindly forwarded
to the outer terminal. Resizes and output share one timestamped canonical capture.

Interactive recording reuses the ordinary attach terminal backend and retained
scene renderer. Only successfully flushed frames advance presentation baselines;
unchanged grids do not repaint. Presentation waits are separately negotiated
through `bmux.recording.process.presentation.wait.v1` (backed by
`bmux.pane_runtime.presentation.wait.v1`), without changing presentation v1 or
durable process v4. Bounded receiving and status connections do not serialize
keyboard input behind screen polling or durable status reconciliation. A bounded
FIFO control queue coalesces only already queued adjacent input bytes into existing
durable input mutations; it adds no collection timer and never crosses a resize
barrier. Uncertain input outcomes are reported rather than automatically retried.

Normal completion restores the outer terminal, waits for real capture EOF and
durable recording completion, publishes requested artifacts, and requests shutdown
of the owned isolated server. Target exit and terminal-owner retirement alone do
not complete capture: a surviving slave descriptor can keep it pending and produce
late output. Pending capture must remain explicitly observable; it is not permission
to keep the outer terminal unrestored indefinitely or to invent successful completion.
The child exit outcome remains distinct from capture, restoration, execution cleanup,
and server-cleanup outcomes. `--json` prints the final structured result
after interactive rendering; otherwise the evidence location is reported on stderr.

A disconnect, transport error, or failed restoration is not permission to kill the
recorded process. Preserve the reported run identity and use the headless status,
reconcile, terminate, and finish commands below to recover deliberately. Outer
suspend/resume is unsupported: suspension requests leave the interactive frontend
with an explicit recovery error rather than pretending job-control continuity.
Recordings may contain credentials and other sensitive input or output.

## Headless recorded processes

`bmux recording process` runs a program in a recorded PTY without requiring an
interactive BMUX attachment. Recording owns the command surface; the process
and recording have separate lifetimes. A detached CLI invocation is not a reason
to terminate the process.

Commands emit JSON results; diagnostics go to stderr. Exit 124 denotes a wait
timeout; foreground `run` propagates the child exit code, capped at 255, while
preserving its JSON result. Automatic isolated startup/routing is an integration
acceptance requirement; manually starting a sandbox is not a substitute.

## Start and inspect

Use an explicit executable and argument vector. BMUX does not implicitly evaluate
a shell command. Shell syntax is interpreted only if you deliberately invoke a
shell, for example `/bin/sh -c '…'`.

```sh
bmux recording process start --json \
  --profile functional --viewport 100x30 --idempotency-key "$START_KEY" \
  --cwd /absolute/project --env MODE=demo \
  -- /absolute/path/to/program 'one argument with spaces' '--program-option'
```

Generate each key once (for example, `START_KEY=$(uuidgen)`) and preserve it with
the exact request before sending. Never regenerate a retry key.

Preserve the returned run identity. Use that run's UUID (or a recording UUID only
where the command documents that selector), never a PID, label, or terminal index.
The `full`, `functional`, and `visual` recording profiles describe capture policy;
check supported exports before choosing a reduced profile. A viewport is terminal
geometry, not pixel dimensions.

```sh
bmux recording process status "$RUN_ID" --json
bmux recording process read "$RUN_ID" --max-bytes 16384 --json
bmux recording process screen "$RUN_ID" --json
```

`read` observes retained output; `screen` observes the current terminal projection.
Neither is a replacement for a byte-exact raw artifact. Continue bounded reads
with `--since "$CURSOR"` using the returned continuation cursor. Keep cursors scoped
to their run; do not invent cursors, reuse another run's cursor, or assume that
an empty bounded read proves process exit or output drain.

## Interactive input and waits

```sh
bmux recording process wait "$RUN_ID" --screen-contains 'Ready>' --timeout 10s --json
bmux recording process send "$RUN_ID" --text 'hello' --idempotency-key "$TEXT_KEY" --json
bmux recording process send "$RUN_ID" --key Enter --idempotency-key "$ENTER_KEY" --json
bmux recording process wait "$RUN_ID" --screen-contains 'hello' --timeout 10s --json
printf 'another line\n' | bmux recording process send "$RUN_ID" --stdin --json
bmux recording process wait "$RUN_ID" --exit --timeout 10s --json
```

Sending text is not the same as pressing Enter. Use an explicit key or deliberately
include a newline. Preserve a UUID `--idempotency-key` for each logical start/run/send.
Retry an uncertain response only with the same key and exactly the same payload.
Matching retries preserve one outcome; conflicting reuse fails. A new key is a
new effect, not a safe retry. Input is an effect: confirm the target identity.
Capture may include secrets
sent through input or printed by the child; treat recordings and exports as
sensitive data.

A wait timeout is an observation failure, not permission to kill the process.
Inspect status and retained output before deciding to continue or terminate.
Distinguish CLI transport/validation errors from a successfully observed nonzero
child exit. Preserve the structured child exit outcome.

`run` is the foreground convenience path for a single program:

```sh
bmux recording process run --json --profile functional --viewport 100x30 \
  -- /absolute/path/to/program argument
```

Read its documented completion/exit behavior rather than assuming that the CLI
exit code alone describes recording integrity.

## JSON response contract

The consolidated contract uses schema version 4:

- `start`, `status`, exit `wait`, `terminate`, and `run`: direct RunStatus with
  `schema_version`, `run_id`, `launch_mutation_id`, `revision`, `recording_id`,
  `execution`, `runtime_root`, `phase`, `exit_code`, `target_exited`,
  `terminal_session_ended`, `capture_eof`, `output_drained`, `output_watermark`,
  `capture_watermark`, `recording_watermark`, `capture_finalized`,
  `durable_recording_complete`, `timing_complete`, `cleanup_complete`,
  `recovery_required`, and `errors`.
- `read`: `{run_id,since,next_cursor,data:[bytes],complete}`. Cursors are unsigned
  raw-output byte offsets, not event numbers or screen positions.
- `screen`: `{schema_version:4,run_id,text}`.
- Screen-condition wait: `{schema_version:4,run_id,matched}`; timeout adds
  `timed_out:true`, sets `matched:false`, and exits 124.
- `send`: `{schema_version:4,run_id,mutation_id,accepted_bytes,receipt}`.
  A completed write receipt acknowledges the terminal write, not child consumption.
- `finish` without output: direct RunStatus. With exports:
  `{schema_version:4,status,artifacts:[paths...]}`.
- Command errors: `{schema_version:4,ok:false,error}` and exit 1.

Do not conflate target exit, session end, capture integrity, cleanup, or recording
commit. Finalization requires the actual capture boundary and complete durable
canonical recording; raw-only recovery does not prove original event timing.

### Pending capture after session end

`target_exited:true` and `terminal_session_ended:true` can coexist with
`capture_eof:false`, `output_drained:false`, `capture_finalized:false`, and
`durable_recording_complete:false`. This is a legitimate pending capture state,
not evidence that the target is still running or that capture failed. The output
and capture watermarks can continue advancing as surviving descriptor holders
write. An empty read, a stable watermark, a retirement receipt, or an observation
timeout must not promote this state to complete. A failed read or lost capture
instead requires an explicit incomplete/recovery outcome.

`cleanup_complete` acknowledges execution resource cleanup and owner retirement,
not capture EOF, descendant death, durable recording completion, or owned-server
shutdown. It must be consumed independently of the capture fields. In particular,
it cannot authorize stopping the capture reader or finalizing evidence.

The current `wait --exit` command also waits for output drain: it can return 124
with the target already exited while capture is still pending. Inspect the returned
fields rather than interpreting 124 as a running target. `finish` must refuse
pending capture, even after target exit and successful retirement; a bounded wait
is not a successful capture boundary. Keep the run identity and owned runtime
available for later status/read/reconcile and finalization after real fd closure.

Validation gives offline GIF rasterization a separate bounded 120-second command
deadline, as explicitly approved. Launch and input retain their existing deadlines.
The helper has a bounded 10-second startup budget; startup uncertainty does not
permit a second launch. Heavy PTY and recording integration tests reserve scheduler
resources to avoid flooding the host. These budgets do not relax byte, event,
identity, final-frame, or durability assertions.

## Termination and finalization

```sh
bmux recording process terminate "$RUN_ID" --grace-period 5s --json
bmux recording process wait "$RUN_ID" --exit --timeout 10s --json
bmux recording process finish "$RUN_ID" --json \
  --export text --export raw --export events --export gif \
  --output /absolute/path/to/new-artifact-directory
```

Termination explicitly ends owned execution; it does not delete evidence or
implicitly replace finalization. `finish --cleanup-server` explicitly shuts down
the dedicated server after finalization; ordinary finish does not imply shutdown.
Finalized status/read/screen and fresh-directory exports remain available offline.
Cleanup does not discard canonical evidence. For example:

```sh
bmux recording process finish "$RUN_ID" --cleanup-server --json
bmux recording process status "$RUN_ID" --json  # finalized offline status
```

`finish` refuses a running process unless an
explicitly documented termination option is requested. Prefer separate terminate,
wait, and finish calls so their effects are auditable. Successful exit observation
must not authorize finalizing before pending output is durably drained.

Exports are derived from canonical recording data:

- `output.pty`: PTY output bytes without UTF-8 decoding or newline normalization.
  stdout/stderr share the PTY; this is not two reconstructed streams.
- `output.txt`: readable transcript, not a terminal screenshot. ANSI controls are
  removed, CR/BS overwrite the current line, LF commits a line, tabs remain,
  and malformed UTF-8 is replaced. Unicode scalar overwrite semantics are not
  grapheme/display-cell editing semantics.
- `events.jsonl`: structured recording events; binary fields are byte arrays.
- `output.gif`: rendered retained terminal states. Validate by decoding frames,
  not merely checking the file extension. Use the returned artifact paths.

Use a fresh destination under a caller-owned parent. Artifact publication must
not report partial output as complete. If export fails, retain the recording and
retry with a new destination after fixing the cause. An existing destination is
not permission to overwrite it. Retrying export must not relaunch the program or
change the recorded outcome. Verify supported formats/profile combinations and
read all reported artifact paths rather than guessing them.

## Native terminal-session lifetime decision

The user explicitly approved a breaking API consolidation: retire old execution
and process API support, reject old clients clearly, and preserve existing recorded
data. Execution and recording each expose one cohesive current contract;
conventional terminal-session lifetime is the default, and recovery is part of the
same contract. Version numbers identify wire/storage representations rather than
user-selectable lifecycle modes. This is a scoped architectural decision, not an
exception to capture completeness, mutation authority, persistence, or isolation.
The required validation includes the standard full suite, CLI tests, PTY smoke,
and terminal compatibility matrix; source changes alone do not establish a pass.
Real installed-payload acceptance remains a separate validation requirement. Unsupported
representations must be explicitly rejected; migrations must preserve canonical
recordings rather than silently reinterpret or delete them.

The public model must distinguish target outcome, terminal-session end, actual
capture boundary and watermark, durable recording finalization, and execution
cleanup. None implies all the others. An incomplete or uncertain operation must
remain discoverable and report its recovery state explicitly.

Launch and input mutations require durable admission keys and request fingerprints
before effects. Retries reconcile the same execution and receipt; conflicting key
reuse is rejected and ambiguous launch never means permission to relaunch. Original
source events and timestamps must survive any recovery that claims complete profile
or animated replay evidence. Raw-only recovery cannot invent missing timing or
telemetry and must report that limitation.

Execution authority, terminal lifetime, capture, and finalization are separate
plugin-owned responsibilities:

- A control guardian retains authenticated execution identity, mutation receipts,
  reconnect authority, and its precisely fenced signal scope outside the
  controlling-terminal session.
- A designated terminal owner defines that session's lifetime. Its retirement is
  independent of raw capture EOF and does not wait for every descendant to exit
  or close its slave descriptor. Conventional OS session-end effects apply, but
  BMUX promises neither universal descriptor revocation nor future terminal access.
  Retirement is not a descendant sweep and does not extend signal authority to
  unrelated, escaped, or surviving processes.
- Capture retains the master reader independently of owner retirement and durably
  records actual kernel bytes, including late bytes from surviving slave holders.
  Target exit, terminal-owner exit, an empty group, and a timeout are not
  substitutes for a capture boundary or proof of byte completeness.
- Finalization requires the target outcome, acknowledged capture boundary, and
  required durable recording commit. Execution cleanup remains a separate outcome.
  Lost or uncertain capture stays explicitly incomplete.

On Linux, a descendant holding a slave descriptor can survive terminal-owner exit
and write after retirement. While such a holder keeps the PTY open, capture stays
pending until actual kernel EOF (including the platform's terminal-closure read
result), and all observed late bytes belong to the canonical recording. BMUX must
not manufacture EOF, close the master to force completion, kill descendants to
obtain EOF, or turn a timeout into successful finalization.

On native macOS, opening `/dev/tty` can retain a hidden controlling-terminal vnode
reference until session-leader exit, after ordinary slave descriptors close.
Keeping the terminal owner alive solely for fencing can therefore prevent EOF.
Conversely, retiring it can revoke descriptors held by escaped descendants or
queued descriptor transfers. These are platform effects, not a portable BMUX
revocation guarantee. Process/descriptor snapshots cannot prove atomic holder
quiescence, and Linux's surviving-holder behavior must not be inferred away from
macOS behavior (or vice versa). Other platforms must report their actual observed
capture boundary or an explicit unsupported/incomplete outcome.

The contract permits conventional session-end effects on future terminal use;
it does **not** permit silently discarding already-written bytes or reporting
truncated evidence complete. Buffered-output preservation must pass owned burst,
small unread-tail, and backpressure tests, as well as Linux surviving-holder late
output coverage. Owner retirement and capture completion require independent
observations. Failure to establish safe draining requires an incomplete/recovery
outcome rather than forced success; legitimate open capture remains pending. Under the
explicitly approved breaking consolidation, the pinned-terminal contract is
retired rather than silently redirected to conventional terminal-session behavior.

There is no implicit pipe substitution or guest execution. Native executables,
query/reply behavior, job control, resizing, retry identity, and isolation remain
acceptance requirements. Changed capabilities must be selected explicitly and
unsupported versions rejected before launch.

## Isolated automation

Never run acceptance tests against an ordinary user endpoint. Use the current
explicitly built binary, not `bmux` from PATH. Create a unique owned root with
config/runtime/data/state/log/recordings/tmp/home directories. Remove **all**
inherited `BMUX_*` variables before setting isolated directory overrides,
`BMUX_NO_BASE_CONFIG=1`, `BMUX_RUNTIME_NAME=default`, and `BMUX_TARGET=local`.
Also isolate HOME, XDG directories, and TMPDIR, and select explicit local runtime
and configuration arguments. Automatic start creates a private
`r-<random>` dedicated root under the selected platform `TMPDIR` (0700) with a durable locator in the
isolated caller's data root. Validate this locator and exact returned root before
routing. Never fall back to default user paths. Cleanup may stop only that owned
runtime, not an arbitrary path or PID.

The black-box acceptance suite is
`packages/cli/tests/recording_process_integration.rs`. It uses
`CARGO_BIN_EXE_bmux`, private test roots, bounded subprocess deadlines, file-backed
command output to avoid pipe-buffer deadlocks, and identity-scoped cleanup attempts.
Cleanup explicitly uses `finish --cleanup-server`; failed cleanup retains the
fixture root for recovery. Tests check dedicated endpoint shutdown and offline
evidence availability. Immediate-output GIF coverage does not poll the screen
before exit: canonical raw-event parser replay must preserve that output.
The coordinator runs builds and tests inside the Nix-selected toolchain:

```sh
bash scripts/dev.sh cargo test -p bmux_cli --test recording_process_integration
```

Full repository validation remains governed by `AGENTS.md`; this focused command
is not a substitute for clippy, nextest, dependency checks, and applicable runtime
and plugin checks.
