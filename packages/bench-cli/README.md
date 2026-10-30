# bmux-bench

A standalone, daemon-free CLI and reusable frontend for BMUX-owned portable
terminal workloads. It tests the terminal you run it inside: no BMUX server,
plugin, remote service, or runtime mutation is needed.

## Commands

```sh
bmux-bench list
bmux-bench list --json
bmux-bench explain plain-logs
bmux-bench run --profile quick # artifacts retained by default
bmux-bench run --internal --profile quick --output results-internal
bmux-bench run --category unicode --samples 12 --label terminal-a --output results-a
bmux-bench run --case plain-logs --case paced-updates --seed 42 --plain
bmux-bench run --profile standard --probe --json --output results-machine
bmux-bench compare results-a results-b
bmux-bench compare results-a results-b --json
bmux-bench report results-a --format markdown
bmux-bench report results-a --format json
```

Real-terminal runs require a controlling terminal. Redirecting stdout is
supported, but never
turns the benchmark into a pipe/sink benchmark: measurement bytes and cursor
queries go to `/dev/tty`. Without a controlling terminal, `run` fails
explicitly.
`list`, `explain`, `report`, and `compare` need no terminal.
With the default-enabled `internal` Cargo feature, `run --internal` invokes
in-process BMUX primitive benchmarks without opening a terminal/backend. These
measure internal computation, not terminal write acceptance or visible
rendering.
Builds without that feature do not expose the flag. Current real-terminal
backend and atomic bundle publication support is platform-dependent; unsupported
platforms return errors rather than pretending to benchmark stdout.

### Selection and overrides

`run` defaults to `--profile quick`. `--category NAME` selects a catalog suite
or
category (repeatable); `--case ID` selects exact cases (repeatable). An explicit
case list without `--category` clears implicit profile suites. With both flags,
selection is their union, followed by profile exclusions, in stable catalog
order.
Synchronized cases require explicit opt-in through configuration or
`--synchronized-updates`; suite selection does not implicitly enable them.

Additional overrides:

- `--samples N`, `--seed N`, `--columns N`, `--rows N`
- `--probe` / `--probe=false`
- `--synchronized-updates` / `--synchronized-updates=false`
- `--config FILE`, `--label TEXT`, `--output NEW_DIRECTORY`, `--no-artifacts`
- `--plain`, `--json`

Boolean values must use `=`. Omitted boolean flags preserve profile settings.
All overrides are revalidated before acquiring the terminal. Geometry must be
2–512 columns, 2–256 rows, at most 65,536 cells; samples must be 1–100.
Requested
geometry must also fit the actual terminal. The core enforces additional
workload,
byte, warmup, and timeout caps. Labels are limited to 256 bytes without control
characters. Unknown flags, profiles, suites, cases, TOML fields, unsupported
schema
versions, and invalid numeric values fail explicitly.

## Profile TOML

```toml
[profiles.quick]
samples = 5
warmup = 1
probe = false

[profiles.my-terminal]
sampling = "quick"
cases = ["plain-logs", "paced-updates"]
exclude = []
samples = 9
warmup = 1
seed = 42
geometry = { columns = 80, rows = 24 }
max_case_ms = 2000
max_bytes_per_sample = 1048576
probe_timeout_ms = 100
probe = false
synchronized_updates = false
practical_change_percent = 5.0
```

```sh
bmux-bench run --config profiles.toml --profile my-terminal --samples 12
```

Profiles inherit their named built-in, or `standard` for a new profile name.
`sampling` changes sampling defaults; explicit profile fields override those
values. Lists replace inherited lists. A configured `cases` list without
`suites`
is an exact selection. CLI overrides apply last. Configuration is bounded to
65,536 bytes and every declared profile is validated, not only the selected one.
Use `list` and `explain` for current identifiers rather than guessing workload
names. Profile TOML is not an artifact format.

## Presentation, output, cancellation

The optional colorful BMUX inline presentation uses `InlineTerminal` and the
components theme, never an alternate screen. It owns no authoritative benchmark
state. Mode-owning announcements finish before measurement owns the terminal.
During
real-terminal runs, synchronous runner callbacks emit category/case/phase/status
rows using BMUX's inline encoder, outside timed sample writes, pacing and
probes.
Consecutive identical phases are coalesced. No background progress task or input
event polling competes with measurement. Progress uses explicit CRLF and never
changes termios while the backend owns it. Progress output failure requests
cancellation and returns an error after backend restoration, never success.
Internal runs have only pre/post-run announcements because their API has no
progress callback. The frontend explicitly calls backend restoration before
artifacts or final printing. Restoration failures are errors, not a success
message.

`--plain`, `NO_COLOR` (including an empty value), redirected stdout, or
nonterminal
stdin disable decoration. `--json` disables decoration and emits one JSON
`RunRecord` on stdout after restoration; actual benchmark traffic still goes to
`/dev/tty`. Errors are returned by the library and printed to stderr by the
binary.
Non-run `--json` commands emit their own single JSON value: catalog, case, or
comparison list. `report --format json` emits the derived `RunSummary`.

On Unix, `run_from` temporarily owns SIGINT, SIGTERM, SIGHUP and SIGTSTP
through a serialized scoped guard. The handler only sets a static lock-free
atomic; a small bridge thread forwards cancellation to the runner's flag. No
terminal cleanup, allocation, locking or nonlocal control transfer occurs inside
the handler. Cancellation is observed at runner checkpoints; bounded writes or
probes may finish or reach their timeout first. **SIGTSTP cancels rather than
suspends** so terminal restoration is not bypassed. Results are explicitly
cancelled (exit 130); partial observations may be retained. SIGKILL, SIGSTOP,
process abort and power loss cannot run cleanup.

The exact previous signal actions (handlers, masks and flags) are restored on
normal return, errors and unwinding. Explicit restoration failures are errors;
unwind cleanup is best-effort and failed ownership blocks future scoped owners.
Concurrent scoped owners are rejected, not overwritten. Process-wide signal
policy cannot be safely shared with unrelated libraries mutating dispositions:
embedders should use `run_from_with_cancel` instead. That API does not inspect
or
change signals, and the caller must set its `Arc<AtomicBool>` to cancel. On
non-Unix platforms, `run_from` rejects a run requiring scoped signal ownership;
use caller-owned cancellation (subject to backend/platform support). Read-only
catalog/report/compare commands do not acquire a signal guard on any platform.
Exit codes: 0 complete/success, 1 noncomplete run, 130 cancelled run, 2 frontend
error. `compare` returns 0 when analysis succeeds even if individual results are
incompatible or inconclusive; inspect the comparison classifications.

## Measurement contracts and statistics

**Accepted writes are not completed visible rendering.** Cursor-position replies
measure protocol response, not a display fence. `--probe` is opt-in: terminal
protocols have no cancellation handshake for an outstanding query. An unresolved
reply disables further queries, gets a bounded quarantine attempt, and causes an
explicit cleanup error rather than a claim of successful reply cleanup.
Extremely
late replies cannot be ruled out; avoid probes on untrusted/unbounded
transports.
Normal portable defaults detect the current viewport; explicit dimensions and
profile geometry are enforced as preconditions.

Paced elapsed time includes
intentional waits; emission deadline misses are not observed dropped display
frames. Synchronized-update framing does not establish atomic visible rendering.
There is no framebuffer capture, camera, compositor presentation timestamp, or
end-to-end keystroke-to-pixel sensor in this frontend.

Human output groups outcomes by category and includes sample observations,
median, MAD, confidence interval, warnings, and artifact location. A case may
have
multiple metrics per measurement iteration. Warmups are not reported as samples.
Analysis uses independent percentile bootstrap median intervals (2,000
resamples,
central 95%), deterministic seeded resampling, and retains outlier observations.
Comparisons report compatibility and uncertainty per metric; percent changes
are `100 * (candidate / baseline - 1)`. Practical thresholds and metric
direction
matter; a percentage alone is not a universal performance score. Tiny sample
counts, clock resolution, scheduling, SSH, multiplexers, terminal size and
terminal
settings limit interpretation. Keep geometry, seed, workload revision/payload,
probe/synchronization settings and sampling conditions comparable. A diagnostic
hypothesis in case metadata is not a causal finding.

## Artifacts

Every successful runner result (including partial/cancelled records) is retained
by default under `bmux-bench-results/<sanitized-run-id>-<unique-suffix>`
relative
to the current directory. The default root is created automatically. `--output
PATH` overrides the destination; its parent must already exist. Existing
destinations are never overwritten. `--no-artifacts` explicitly opts out and
conflicts with `--output`. Artifact failures return an error instead of silently
discarding results. Human output prints the chosen bundle path. Machine stdout
remains exactly the `RunRecord`, without an extra path/status object; default
bundles can be located under the results root by run ID. Bundle publication is a
core-managed, synchronized,
no-replace operation; a failed publication may leave a hidden staging directory,
not a valid completed result.

Schema version 1 bundles contain:

- `checksums.json`: schema version and exact file byte lengths/SHA-256 digests
- `run.json`: versioned `RunRecord`, configuration, allowlisted environment and
  capabilities, stable case order, per-case status/reason/hash and raw samples
- `samples.jsonl`: raw sample observations, one JSON object per line
- `summary.json`: derived metric statistics and method
- `report.md`: derived human-readable Markdown report

Samples identify case/index/metric/unit plus elapsed nanoseconds, accepted
bytes,
logical operations, write calls and emission deadline misses (internal metrics
retain their own units and measurement metadata). Case revision,
payload hash, run identity, harness version, seed and geometry support audit and
compatibility checks. Readers verify bounded input and bundle integrity/schema;
unsupported/corrupt state is an error, not an empty healthy result. Manifest
hashes
provide integrity, not publisher authentication. Private credentials and
wholesale
environment capture are not part of the contract.

## Library embedding

```rust,no_run
let code = bmux_bench_cli::run_from([
    "bmux-bench", "report", "results-a", "--format", "json",
])?;
// The embedding application owns process exit and error presentation.
# Ok::<(), String>(())
```

`run() -> Result<u8, String>` consumes process argv;
`run_from<I, T>(args: I) -> Result<u8, String>` accepts clap-compatible
arguments
including a program name (`T: Into<OsString> + Clone`). It does not exit the
process
or launch a daemon. Help/version return success; parse/execution errors return
`Err`. Calls that benchmark a terminal require exclusive ownership of its output
and modes. `run_from_with_cancel(args, Arc<AtomicBool>) -> Result<u8, String>`
is the
embedding entrypoint for caller-owned cancellation and leaves all process signal
dispositions untouched. Its flag is never reset by the frontend. The scoped
`run_from` implementation uses libc and a standard-library bridge thread, not
Tokio signal registration. Do not invoke simultaneous terminal runs or mix other
terminal writers with a run.
