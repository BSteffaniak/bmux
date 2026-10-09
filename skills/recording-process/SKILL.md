---
name: recording-process
description: Run and inspect an explicitly owned recorded PTY process, interact with bounded reads and waits, then finalize evidence without touching a live user runtime.
---

# recording-process

Use this recipe for reproducible command recordings or interactive terminal
investigation. This workflow launches processes, sends input, and writes artifacts;
it is not read-only. Obtain normal tool permission and user authorization for
those effects. Read [recorded-process documentation](../../docs/recording-process.md)
and current CLI help before acting; do not infer a JSON schema from old examples.

## Safety boundary

- Select an explicit current BMUX binary and unique owned isolation root. Never
  use a PATH-installed binary or the ordinary user endpoint for experiments.
- Clear inherited `BMUX_*` settings, including configuration, target, attachment,
  runtime, recordings, and slot selectors. Isolate HOME, XDG directories, TMPDIR,
  config/runtime/data/state/log/recordings paths; disable base configuration and
  select the isolated default/local runtime explicitly.
- Do not build, rebuild plugins, stop servers, or delete shared outputs when a
  coordinator owns those resources. Ask the coordinator for the built binary.
- Confirm the durable locator is in the caller-owned data root. Automatic start
  creates a private `/tmp/bmux-recorded-UUID` dedicated root (0700); validate that
  exact root before routing. Missing endpoints must fail, never fall back to a
  live user runtime.
- Keep returned resource identities and cursors exactly; do not substitute a PID,
  index, label, another run's cursor, or an arbitrary UUID found in diagnostics.
- Recording can capture credentials and private output. Do not publish evidence
  without reviewing/redacting a separate copy; preserve the canonical original.

## Recipe

1. Inspect `recording process --help`, plus the leaf command help. Confirm available
   profiles, JSON response shape, cursor representation, and supported exports.
   Status results are direct RunStatus; screen uses `text`; read uses `data` byte
   arrays and unsigned byte-offset `next_cursor`; export finish wraps RunStatus
   under `status` and reports `artifacts`. Screen wait reports `matched`, with
   `timed_out:true` and CLI exit 124 on timeout. Foreground run preserves JSON
   while propagating child exit status (capped at 255).
2. Start with explicit argv, bounded viewport, cwd, and intentional environment:

   ```sh
   "$BMUX_BIN" recording process start --json --profile functional \
     --viewport 100x30 --cwd "$PROJECT" --env MODE=demo --idempotency-key "$START_KEY" \
     -- /absolute/path/to/program 'literal argument'
   ```

   Generate each UUID key once and preserve it with the exact request before
   sending; never regenerate a retry key. Apply the isolation environment and explicit local config/runtime selection
   to **every** invocation, not just start. There is no implicit shell expansion.
   Invoke a shell explicitly only when shell semantics are intended.
3. Save the documented run ID and recording ID separately. Check `status --json`.
   Handle CLI errors separately from an observed nonzero child exit.
4. Wait for a readiness condition before sending input:

   ```sh
   "$BMUX_BIN" recording process wait "$RUN_ID" \
     --screen-contains 'Ready>' --timeout 10s --json
   "$BMUX_BIN" recording process send "$RUN_ID" --text 'hello' --json
   "$BMUX_BIN" recording process send "$RUN_ID" --key Enter --json
   ```

   Preserve a UUID `--idempotency-key` for each logical start/run/send. Retry an
   uncertain response only with that same key and identical arguments/input;
   conflicting reuse must fail. A fresh key authorizes another effect. Inspect
   output before deciding to send new input. `--stdin` is an alternative for intentional
   byte/newline input, not a reason to stream unbounded secrets.
5. Read narrowly: `read "$RUN_ID" --max-bytes 16384 --json`; continue with the
   returned `--since` cursor. Use `screen --json` for authoritative terminal
   state. Empty reads do not prove completion. Keep an external deadline for
   every subprocess and limit total evidence retained in the agent context.
6. Wait with `--exit --timeout 10s`. A timeout does not terminate the child.
   If ending work is authorized, explicitly `terminate --grace-period 5s`,
   observe exit/drain, and only then finish. Never use finish as an implicit kill.
7. Finish to a fresh caller-owned directory:

   ```sh
   "$BMUX_BIN" recording process finish "$RUN_ID" --json \
     --export text --export raw --export events --export gif --output "$OUTPUT"
   ```

   Check supported profile/format combinations first. On export failure, preserve
   recording identity and retry a new destination; do not relaunch the program,
   overwrite existing evidence, or claim partial output as successful.
8. Verify evidence: raw bytes exactly; transcript with Unicode/ANSI/CR semantics;
   each events JSONL record; decoded GIF dimensions, frames, delays, and visible
   changes. A GIF header or nonempty file alone is not validation.
9. Use `finish --cleanup-server` to explicitly stop only the owned dedicated
   server after finalization. Finalized status/read/screen and fresh-directory
   exports must remain available offline. Retain roots on cleanup failure.
   A detached command does not authorize terminating unrelated execution.

## Report

Return run/recording identities, executable and argv, isolation root, observed
child outcome, timeout/termination/finalization outcomes separately, verified
artifact paths, and any evidence gaps. Never claim successful completion from
CLI admission, idle state, or a partial export. Do not dump secret environment
values or entire unbounded transcripts.
