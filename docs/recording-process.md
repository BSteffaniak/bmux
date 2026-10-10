# Recorded processes

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

- `start`, `status`, exit `wait`, `terminate`, and `run`: direct RunStatus with
  `schema_version`, `run_id`, `revision`, `recording_id`, `execution`, `runtime_root`,
  `phase`, `exit_code`, `output_drained`, `output_watermark`, `capture_finalized`,
  and `errors`.
- `read`: `{run_id,since,next_cursor,data:[bytes],complete}`. Cursors are unsigned
  raw-output byte offsets, not event numbers or screen positions.
- `screen`: `{schema_version:1,run_id,text}`.
- Screen-condition wait: `{schema_version:1,run_id,matched}`; timeout adds
  `timed_out:true`, sets `matched:false`, and exits 124.
- `send`: `{schema_version:1,run_id,accepted_bytes}`.
- `finish` without output: direct RunStatus. With exports:
  `{schema_version:1,status,artifacts:[paths...]}`.
- Command errors: `{schema_version:1,ok:false,error}` and exit 1.

Do not conflate `exit_code` with capture integrity: require `output_drained` before
finalization and `capture_finalized` before treating the recording as finalized.

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

The accepted direction is a separately negotiated conventional terminal-session
contract. It is not a reinterpretation of `recording-process-v1` or the existing
preserve-descendants execution policy. The new contract is under implementation;
examples above still describe v1. The opt-in CLI selection is the paired
`--contract-version 3 --lifetime terminal-session` flags on `start` or `run`.
Its native service is `recording-process-terminal-v3`; it remains subject to
end-to-end validation and does not advertise durable daemon recovery.

For this selection, JSON wraps the operation result in an explicit envelope:

```json
{"contract":{"interface":"recording-process-terminal-v3","lifecycle":"conventional-terminal-session","status_schema_version":1},"result":{}}
```

`result` retains the existing operation's modeled field shape and schema version.
The default v1 commands still return their existing bare JSON. Capture readiness
(`output_drained`) does not claim descendant cleanup: native readiness requires
session end, complete capture, and the actual raw capture boundary. An incomplete
or recovery-required native status returns an explicit error.

Native launch persists its key, selected contract, and isolated runtime locator
before invoking execution. An ambiguous response is not retried as a fresh launch
and does not implicitly terminate possibly running work. The error identifies the
retained key, exact runtime root, and `launch-KEY-runtime.json` recovery record;
known run identity is retained when available. These records support explicit
investigation, not an advertised automatic daemon-recovery capability. Original
recording sinks preserve uninterrupted capture timing; raw-only recovered bytes
must not be presented as recovered original event timing or full telemetry.

Execution authority, terminal lifetime, capture, and finalization are separate
plugin-owned responsibilities:

- A control guardian retains authenticated execution identity, mutation receipts,
  reconnect authority, and its precisely fenced signal scope outside the
  controlling-terminal session.
- A designated terminal owner defines that session's lifetime. At session end,
  normal OS hangup/revocation semantics apply. BMUX does not thereby gain
  authority to kill unrelated or escaped processes; continued access to the ended
  terminal is not guaranteed by this new contract.
- Capture retains the master reader and durably records actual kernel bytes.
  Target exit, terminal-owner exit, an empty group, and a timeout are not
  substitutes for a capture boundary or proof of byte completeness.
- Finalization requires the target outcome, acknowledged capture boundary, and
  required durable recording commit. Execution cleanup remains a separate outcome.
  Lost or uncertain capture stays explicitly incomplete.

On native macOS, opening `/dev/tty` can retain a hidden controlling-terminal vnode
reference until session-leader exit, after ordinary slave descriptors close.
Keeping the terminal owner alive solely for fencing can therefore prevent EOF.
Conversely, retiring it can revoke descriptors held by escaped descendants or
queued descriptor transfers. Process/descriptor snapshots cannot prove atomic
holder quiescence.

The new contract permits conventional session-end effects on future terminal use;
it does **not** permit silently discarding already-written bytes or reporting
truncated evidence complete. Buffered-output preservation must pass owned burst,
small unread-tail, and backpressure tests. Failure to establish safe draining
requires an incomplete/recovery outcome rather than forced success. Legacy
semantics stay separately supported without target-exit-based automatic retirement.

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
`/tmp/bmux-recorded-UUID` dedicated root (0700) with a durable locator in the
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
