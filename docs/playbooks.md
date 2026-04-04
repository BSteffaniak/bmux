# Playbook System Reference

Playbooks are headless, scriptable bmux sessions. A playbook defines a sequence
of actions (create sessions, send keystrokes, assert screen content) that bmux
executes against an ephemeral sandbox server and reports pass/fail results as
structured JSON.

**Primary use cases:**

- LLM-driven validation: generate playbooks from bug descriptions, run them to
  reproduce and verify fixes without manual screen recordings.
- CI regression tests: deterministic, repeatable terminal interaction tests.
- Recording conversion: turn a captured bmux session into a re-runnable test.

**Execution model:** By default, `bmux playbook run` spawns an isolated sandbox
server in a temp directory, executes all steps, reports results, and tears down
the server. Use `--target-server` to run against a live server instead.

**Two input formats** parse into the same internal representation:

| Format            | Extension        | Typical use                             |
| ----------------- | ---------------- | --------------------------------------- |
| Line-oriented DSL | `.dsl` or stdin  | Quick authoring, LLM generation, piping |
| TOML              | `.playbook.toml` | Structured config, version control      |

---

## CLI Commands

### `bmux playbook run`

Run a playbook and report results.

```
bmux playbook run <source> [flags]
```

| Argument/Flag            | Type   | Default  | Description                                      |
| ------------------------ | ------ | -------- | ------------------------------------------------ |
| `<source>`               | string | required | Path to playbook file, or `-` for stdin          |
| `--json`                 | bool   | false    | Output results as JSON to stdout                 |
| `--interactive`          | bool   | false    | Pause before each step for interactive control   |
| `--target-server`        | bool   | false    | Run against the live server instead of a sandbox |
| `--record`               | bool   | false    | Record the execution (overrides playbook config) |
| `--export-gif <path>`    | string | none     | Export recording as GIF (implies `--record`)     |
| `--viewport <COLSxROWS>` | string | none     | Override viewport dimensions (e.g. `120x40`)     |
| `--timeout <secs>`       | u64    | none     | Override max playbook timeout in seconds         |
| `--shell <path>`         | string | none     | Override shell binary                            |
| `--var KEY=VALUE`        | string | none     | Define a variable (repeatable, overrides `@var`) |
| `--verbose` / `-v`       | bool   | false    | Print step-by-step progress to stderr            |

**Exit codes:** `0` = all steps passed, `1` = one or more steps failed or error.

**Stdin example:**

```sh
echo 'new-session\nsend-keys keys="echo hi\\r"\nwait-for pattern="hi"' | bmux playbook run - --json
```

**Interactive live tour:**

Use `--interactive` from a real terminal (TTY) to enter a full-screen live tour
that continuously renders pane output while the playbook runs.

- `space`: pause/resume live playback
- `n`: single-step one playbook step (when paused)
- `c` / `l`: return to live running mode
- `:<dsl>`: run an ad-hoc DSL action at step boundaries
- `q`: abort run (remaining scheduled steps are marked skipped)
- `?`: show control help in the status line

If stdin/stdout are not TTYs (for example piped input in CI), `--interactive`
automatically falls back to the line-prompt controls.

### `bmux playbook validate`

Parse and validate a playbook without executing it.

```
bmux playbook validate <source> [--json]
```

Returns validation errors (missing `new-session` as first step, unknown actions, etc.).

### `bmux playbook dry-run`

Parse, validate, and print the execution plan without running.

```
bmux playbook dry-run <source> [--json]
```

| Argument/Flag | Type   | Default  | Description                             |
| ------------- | ------ | -------- | --------------------------------------- |
| `<source>`    | string | required | Path to playbook file, or `-` for stdin |
| `--json`      | bool   | false    | Output as structured JSON               |

**Exit codes:** `0` = playbook is valid, `1` = validation errors found.

**JSON output:**

```json
{
  "valid": true,
  "config": {
    "name": "my-test",
    "viewport": "80x24",
    "shell": "sh",
    "timeout_ms": 30000,
    "env_mode": "default",
    "record": false
  },
  "steps": [
    { "index": 0, "action": "new-session", "dsl": "new-session" },
    { "index": 1, "action": "send-keys", "dsl": "send-keys keys='echo hi\\r'" },
    { "index": 2, "action": "wait-for", "dsl": "wait-for pattern='hi'" }
  ],
  "step_count": 3,
  "errors": []
}
```

Each step's `dsl` field contains the round-trip DSL serialization of the action,
which is valid DSL syntax that can be copy-pasted.

### `bmux playbook diff`

Compare results from two playbook runs. Produces a structured diff covering
step status changes, screen text differences, timing comparison, and failure
capture comparison.

```
bmux playbook diff <left.json> <right.json> [flags]
```

| Argument/Flag | Type | Default | Description |
|---------------|------|---------|-------------|
| `<left.json>` | string | required | Path to baseline/left playbook result JSON |
| `<right.json>` | string | required | Path to new/right playbook result JSON |
| `--json` | bool | false | Output diff as structured JSON |
| `--timing-threshold <pct>` | u64 | 50 | Flag steps that slowed by more than this percent |

**Exit codes:** `0` = no changes detected, `1` = changes or regressions found.

**JSON output includes:**

- `summary` -- outcome change, step/snapshot counts, total timing delta
- `step_diffs` -- per-step status changes, timing deltas, detail/expected/actual on failures
- `snapshot_diffs` -- per-snapshot pane text diffs (unified diff format via Myers algorithm)
- `failure_capture_diffs` -- screen state diffs from auto-snapshots on failure
- `timing_regressions` -- steps that exceeded the timing threshold

**Usage pattern for before/after verification:**
```sh
# Run before fix
bmux playbook run --json test.dsl > before.json
# Apply fix...
bmux playbook run --json test.dsl > after.json
# Compare
bmux playbook diff --json before.json after.json
```

### `bmux playbook cleanup`

Remove orphaned sandbox temp directories from previous playbook runs. Useful
after SIGKILL or crashes that prevent normal cleanup.

```
bmux playbook cleanup [--dry-run] [--json]
```

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--dry-run` | bool | false | List orphaned dirs without deleting |
| `--json` | bool | false | Output as JSON |

Detection heuristic: directories matching `bpb-*` in the system temp dir that
are older than 5 minutes and whose server process is no longer running.

### `bmux playbook interactive`

Start an interactive playbook session with a socket for agent control.

```
bmux playbook interactive [flags]
```

| Flag                     | Type   | Default        | Description          |
| ------------------------ | ------ | -------------- | -------------------- |
| `--socket <path>`        | string | auto           | Socket path override |
| `--record`               | bool   | false          | Record the session   |
| `--viewport <COLSxROWS>` | string | `80x24`        | Viewport dimensions  |
| `--shell <path>`         | string | system default | Shell binary         |
| `--timeout <secs>`       | u64    | no limit       | Max session lifetime |

See [Interactive Mode Protocol](#interactive-mode-protocol) for the wire format.

### `bmux playbook from-recording`

Generate a playbook from an existing recording.

```
bmux playbook from-recording <recording-id> [--output <path>]
```

If `--output` is omitted, writes to stdout. The generated playbook includes
`wait-for` barriers and `assert-screen` checks derived from the recorded output.
See [Recording to Playbook Conversion](#recording-to-playbook-conversion).

---

## DSL Format

Each line is one of:

| Line type          | Prefix      | Example                      |
| ------------------ | ----------- | ---------------------------- |
| Blank / whitespace | (empty)     | Ignored                      |
| Comment            | `#`         | `# this is a comment`        |
| Config directive   | `@`         | `@viewport cols=80 rows=24`  |
| Action             | action name | `send-keys keys='echo hi\r'` |

### Argument Format

Actions and directives use `key=value` pairs separated by whitespace:

```
action-name key1=value1 key2='value with spaces' key3="also quoted"
```

**Quoting rules:**

| Form          | Example             | Notes                         |
| ------------- | ------------------- | ----------------------------- |
| Bare          | `key=value`         | Terminated by next whitespace |
| Single-quoted | `key='hello world'` | Supports C-style escapes      |
| Double-quoted | `key="hello world"` | Supports C-style escapes      |

**C-style escape sequences** (inside quoted values and `send-keys keys=`):

| Escape | Byte   | Name                 |
| ------ | ------ | -------------------- |
| `\r`   | `0x0D` | Carriage return      |
| `\n`   | `0x0A` | Line feed            |
| `\t`   | `0x09` | Tab                  |
| `\0`   | `0x00` | Null                 |
| `\a`   | `0x07` | Bell                 |
| `\b`   | `0x08` | Backspace            |
| `\e`   | `0x1B` | Escape (ESC)         |
| `\\`   | `0x5C` | Literal backslash    |
| `\'`   | `0x27` | Literal single quote |
| `\"`   | `0x22` | Literal double quote |
| `\xNN` | `0xNN` | Arbitrary hex byte   |

---

## Config Directives

Directives set playbook-wide configuration. They must appear before any action
lines (or be interspersed; order relative to actions does not matter since
directives are processed in a first pass).

| Directive      | Syntax                                          | Default        | Description                                             |
| -------------- | ----------------------------------------------- | -------------- | ------------------------------------------------------- |
| `@viewport`    | `@viewport cols=<u16> rows=<u16>`               | `80x24`        | Terminal viewport dimensions                            |
| `@shell`       | `@shell <path>`                                 | system default | Shell binary for the sandbox                            |
| `@timeout`     | `@timeout <ms>`                                 | `30000`        | Max playbook execution time in milliseconds             |
| `@record`      | `@record true\|false`                           | `false`        | Enable recording of the execution                       |
| `@name`        | `@name <string>`                                | none           | Playbook name (included in JSON output)                 |
| `@description` | `@description <string>`                         | none           | Playbook description                                    |
| `@plugin`      | `@plugin enable=<id>` or `@plugin disable=<id>` | all enabled    | Enable/disable specific plugins                         |
| `@var`         | `@var NAME=VALUE`                               | none           | Define a static variable for `${NAME}` substitution     |
| `@env`         | `@env NAME=VALUE`                               | none           | Set an environment variable in the sandbox process      |
| `@env-mode`    | `@env-mode inherit\|clean`                      | `inherit`      | Sandbox environment isolation mode                      |
| `@include`     | `@include <path>`                               | none           | Include another playbook file (recursive, max depth 10) |

### Environment Modes

| Mode      | Behavior                                                                                                                                                                                                                      |
| --------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `inherit` | Sandbox inherits the full parent environment, then overlays deterministic defaults for `TERM` (`xterm-256color`), `LANG` (`C.UTF-8`), `LC_ALL` (`C.UTF-8`), and `HOME` (sandbox temp dir). `@env` entries are applied on top. |
| `clean`   | Sandbox starts with an empty environment. Only `PATH`, `USER`, and `SHELL` are inherited from the parent. All other variables use deterministic defaults or explicit `@env` entries.                                          |

**Resolution chain:** `@env-mode` in playbook (if set) > `BMUX_PLAYBOOK_ENV_MODE`
environment variable (if set) > `inherit`.

---

## Actions Reference

### Session Lifecycle

#### `new-session`

Create a new session. Must be the first action in a sandbox playbook.

```
new-session [name=<string>]
```

| Arg    | Type   | Required | Default | Description  |
| ------ | ------ | -------- | ------- | ------------ |
| `name` | string | no       | auto    | Session name |

Sets `${SESSION_ID}`, `${SESSION_NAME}`, `${PANE_COUNT}` (=1), `${FOCUSED_PANE}` (=1).

#### `kill-session`

Kill a session by name.

```
kill-session name=<string>
```

| Arg    | Type   | Required | Default | Description  |
| ------ | ------ | -------- | ------- | ------------ |
| `name` | string | yes      | -       | Session name |

### Pane Management

#### `split-pane`

Split the current pane.

```
split-pane [direction=vertical|horizontal|v|h] [ratio=<f64>]
```

| Arg         | Type   | Required | Default               | Description                                         |
| ----------- | ------ | -------- | --------------------- | --------------------------------------------------- |
| `direction` | string | no       | `vertical`            | Split direction. `v`/`vertical` or `h`/`horizontal` |
| `ratio`     | f64    | no       | none (server default) | Split ratio (0.0-1.0)                               |

Increments `${PANE_COUNT}`.

#### `focus-pane`

Change the focused pane.

```
focus-pane target=<u32>
```

| Arg      | Type | Required | Default | Description                   |
| -------- | ---- | -------- | ------- | ----------------------------- |
| `target` | u32  | yes      | -       | Pane index to focus (1-based) |

Updates `${FOCUSED_PANE}`.

#### `close-pane`

Close a pane.

```
close-pane [target=<u32>]
```

| Arg      | Type | Required | Default      | Description                   |
| -------- | ---- | -------- | ------------ | ----------------------------- |
| `target` | u32  | no       | focused pane | Pane index to close (1-based) |

Decrements `${PANE_COUNT}`.

### Input

#### `send-keys`

Send input bytes to a pane. This is the primary way to type commands.

```
send-keys keys=<escaped-string> [pane=<u32>]
```

| Arg    | Type   | Required | Default      | Description                                                                 |
| ------ | ------ | -------- | ------------ | --------------------------------------------------------------------------- |
| `keys` | string | yes      | -            | Input bytes with C-style escapes. Use `\r` for Enter.                       |
| `pane` | u32    | no       | focused pane | Target pane index (1-based). Uses `PaneDirectInput` for race-free delivery. |

**Examples:**

```
send-keys keys='echo hello\r'
send-keys keys='ls -la\r' pane=2
send-keys keys='\x03'                  # Ctrl+C
send-keys keys='\e[A'                  # Up arrow
```

#### `send-bytes`

Send raw bytes specified as a hex string.

```
send-bytes hex=<hex-string>
```

| Arg   | Type   | Required | Default | Description                                       |
| ----- | ------ | -------- | ------- | ------------------------------------------------- |
| `hex` | string | yes      | -       | Hex-encoded bytes (e.g. `1b5b41` for ESC `[` `A`) |

#### `send-attach`

Send a key chord through the attach keybinding runtime (same path as interactive
attach mode). Use this for UI-mode behaviors like scrollback/copy-mode,
keybinding-driven pane focus, and runtime/plugin commands.

```
send-attach key=<chord>
```

| Arg   | Type   | Required | Default | Description                                  |
| ----- | ------ | -------- | ------- | -------------------------------------------- |
| `key` | string | yes      | -       | Key chord string (e.g. `ctrl+a [`, `k`, `esc`) |

#### `prefix-key`

Compatibility alias that sends `Ctrl-A` plus one key via `send-attach`.

```
prefix-key key=<char>
```

| Arg   | Type | Required | Default | Description                               |
| ----- | ---- | -------- | ------- | ----------------------------------------- |
| `key` | char | yes      | -       | Single character to send after the prefix |

Do not mix attach UI-mode entry with `send-keys` for follow-up navigation keys.
`send-keys` writes bytes to the pane shell; `send-attach` runs attach key handling.

```
# Bad: enters scrollback, then types into shell
prefix-key key='['
send-keys keys='k\r'

# Good: all UI-mode keys use attach path
send-attach key='ctrl+a ['
send-attach key='k'
send-attach key='enter'
```

### Synchronization

#### `wait-for`

Poll the screen until a regex pattern matches. This is the primary
synchronization mechanism -- use it after `send-keys` to wait for output before
proceeding.

```
wait-for pattern=<regex> [pane=<u32>] [timeout=<ms>] [retry=<u32>]
```

| Arg       | Type  | Required | Default      | Description                                |
| --------- | ----- | -------- | ------------ | ------------------------------------------ |
| `pattern` | regex | yes      | -            | Regex pattern to match against screen text |
| `pane`    | u32   | no       | focused pane | Pane index (1-based)                       |
| `timeout` | u64   | no       | `5000`       | Max wait time in milliseconds              |
| `retry`   | u32   | no       | `1`          | Number of attempts (1 = no retry)          |

**Polling behavior:** Exponential backoff starting at 10ms, doubling up to
200ms max (10, 20, 40, 80, 160, 200, 200...). Each poll drains output and
refreshes the screen.

**On timeout:** The step fails with an error message that includes the first
200 characters of the current screen text for debugging.

**Pattern tips:**

- Use `\\d+` to match any sequence of digits (PIDs, line numbers, etc.)
- Use `\\$` to match a literal `$` (common in shell prompts)
- The pattern is tested against the full visible screen text of the target pane.

#### `sleep`

Pause execution for a fixed duration. Prefer `wait-for` when possible.

```
sleep ms=<u64>
```

| Arg  | Type | Required | Default | Description              |
| ---- | ---- | -------- | ------- | ------------------------ |
| `ms` | u64  | yes      | -       | Duration in milliseconds |

#### `wait-for-event`

Wait for a server-side event.

```
wait-for-event event=<name> [timeout=<ms>]
```

| Arg       | Type   | Required | Default | Description                   |
| --------- | ------ | -------- | ------- | ----------------------------- |
| `event`   | string | yes      | -       | Event name (exact match)      |
| `timeout` | u64    | no       | `5000`  | Max wait time in milliseconds |

**Supported event names:**

| Event name            | Triggered when                   |
| --------------------- | -------------------------------- |
| `server_started`      | Server finishes startup          |
| `server_stopping`     | Server begins shutdown           |
| `session_created`     | A new session is created         |
| `session_removed`     | A session is destroyed           |
| `client_attached`     | A client attaches to a session   |
| `client_detached`     | A client detaches                |
| `attach_view_changed` | The attached view layout changes |

### Assertions

#### `assert-screen`

Assert conditions on the visible screen text. At least one of `contains`,
`not_contains`, or `matches` is required.

```
assert-screen [pane=<u32>] [contains=<string>] [not_contains=<string>] [matches=<regex>]
```

| Arg            | Type   | Required | Default      | Description                        |
| -------------- | ------ | -------- | ------------ | ---------------------------------- |
| `pane`         | u32    | no       | focused pane | Pane index (1-based)               |
| `contains`     | string | no       | -            | Substring that must be present     |
| `not_contains` | string | no       | -            | Substring that must NOT be present |
| `matches`      | regex  | no       | -            | Regex pattern that must match      |

Checks are evaluated in order: `contains` first, then `not_contains`, then
`matches`. All specified checks must pass.

**On failure:** The error detail includes the full screen text of the target
pane, allowing the caller to see what was actually on screen.

**Examples:**

```
assert-screen contains='hello world'
assert-screen not_contains='error' pane=1
assert-screen matches='total \\d+ files'
assert-screen contains='success' not_contains='failure'
```

#### `assert-layout`

Assert the number of panes.

```
assert-layout pane_count=<u32>
```

| Arg          | Type | Required | Default | Description              |
| ------------ | ---- | -------- | ------- | ------------------------ |
| `pane_count` | u32  | yes      | -       | Expected number of panes |

#### `assert-cursor`

Assert the cursor position in a pane.

```
assert-cursor [pane=<u32>] row=<u16> col=<u16>
```

| Arg    | Type | Required | Default      | Description                      |
| ------ | ---- | -------- | ------------ | -------------------------------- |
| `pane` | u32  | no       | focused pane | Pane index (1-based)             |
| `row`  | u16  | yes      | -            | Expected cursor row (0-based)    |
| `col`  | u16  | yes      | -            | Expected cursor column (0-based) |

### Inspection

#### `snapshot`

Capture the current screen state of all panes. Snapshots are included in the
`PlaybookResult.snapshots` array and in interactive mode responses.

```
snapshot id=<string>
```

| Arg  | Type   | Required | Default | Description                                              |
| ---- | ------ | -------- | ------- | -------------------------------------------------------- |
| `id` | string | yes      | -       | Label for this snapshot (used to identify it in results) |

Each snapshot captures every pane's visible text, cursor position, focus state,
and index.

#### `screen`

Capture and return the current screen state. In batch mode, the step detail
contains JSON-serialized pane captures. In interactive mode, the response
`panes` field is populated.

```
screen
```

No arguments. Useful for LLM debugging -- inspect screen state without asserting.

#### `status`

Query the current session status. Returns session ID, pane count, and focused
pane index in the step detail.

```
status
```

No arguments.

### Layout

#### `resize-viewport`

Change the terminal viewport dimensions.

```
resize-viewport cols=<u16> rows=<u16>
```

| Arg    | Type | Required | Default | Description      |
| ------ | ---- | -------- | ------- | ---------------- |
| `cols` | u16  | yes      | -       | New column count |
| `rows` | u16  | yes      | -       | New row count    |

### Services

#### `invoke-service`

Invoke a plugin service.

```
invoke-service capability=<cap> interface=<id> operation=<op> [kind=query|command] [payload=<json>]
```

| Arg          | Type   | Required | Default   | Description                    |
| ------------ | ------ | -------- | --------- | ------------------------------ |
| `capability` | string | yes      | -         | Plugin capability name         |
| `interface`  | string | yes      | -         | Service interface ID           |
| `operation`  | string | yes      | -         | Operation name                 |
| `kind`       | string | no       | `command` | `query`/`q` or `command`/`cmd` |
| `payload`    | string | no       | `""`      | JSON payload string            |

---

## Step Modifiers

### `!continue` — Continue on Error

Append `!continue` to any action line to prevent the playbook from stopping
if that step fails. The step is still recorded as `fail` in the results, and
`pass` will be `false`, but execution continues to the next step.

```
assert-screen contains='optional_check' !continue
assert-screen contains='required_check'
```

In TOML format, use `continue_on_error = true` on the step:

```toml
[[step]]
action = "assert-screen"
contains = "optional_check"
continue_on_error = true
```

This is useful for diagnostic playbooks that want to check multiple conditions
and report all failures, not just the first one.

## Variable Substitution

Playbook values support `${NAME}` variable references. Variables are resolved at
execution time, not parse time.

### Variable Sources

Variables are resolved in this order (first match wins):

1. **Runtime variables** -- dynamic values set during execution
2. **Static variables** -- defined via `@var` directives
3. **Environment variables** -- from the process environment
4. **Unresolved** -- if no match, `${NAME}` is left as-is (with a warning logged)

### Literal `${` Escaping

Use `$${...}` to produce a literal `${...}` without variable expansion:

```
send-keys keys='echo $${HOME}\r'   # sends literal ${HOME} to the terminal
```

The first `$` acts as an escape character. After resolution, `$${HOME}` becomes
the literal string `${HOME}`.

### Runtime Variables

| Variable          | Type           | Set by                                    | Description          |
| ----------------- | -------------- | ----------------------------------------- | -------------------- |
| `${SESSION_ID}`   | UUID string    | `new-session`                             | Current session UUID |
| `${SESSION_NAME}` | string         | `new-session`                             | Current session name |
| `${PANE_COUNT}`   | integer string | `new-session`, `split-pane`, `close-pane` | Number of panes      |
| `${FOCUSED_PANE}` | integer string | `new-session`, `focus-pane`               | Focused pane index   |

### Static Variables

Defined with `@var`:

```
@var BASE_DIR=/tmp/test
@var MARKER=test_marker_42

send-keys keys='cd ${BASE_DIR}\r'
wait-for pattern='${MARKER}'
```

Static variables take priority over environment variables with the same name.

---

## TOML Format

TOML playbooks use `[playbook]` for config and `[[step]]` for actions.

### `[playbook]` Section

| Field             | Type     | Default        | Description                         |
| ----------------- | -------- | -------------- | ----------------------------------- |
| `name`            | string   | none           | Playbook name                       |
| `description`     | string   | none           | Description                         |
| `viewport.cols`   | u16      | 80             | Viewport columns                    |
| `viewport.rows`   | u16      | 24             | Viewport rows                       |
| `shell`           | string   | system default | Shell binary                        |
| `timeout_ms`      | u64      | 30000          | Max execution time in ms            |
| `record`          | bool     | false          | Enable recording                    |
| `plugins.enable`  | string[] | []             | Plugin IDs to enable                |
| `plugins.disable` | string[] | []             | Plugin IDs to disable               |
| `vars`            | table    | {}             | Static variables (`NAME = "VALUE"`) |
| `env`             | table    | {}             | Environment variables               |
| `env_mode`        | string   | none           | `"inherit"` or `"clean"`            |
| `include`         | string[] | []             | Paths to include                    |

### `[[step]]` Entries

Each step requires an `action` field. Other fields are action-specific:

```toml
[[step]]
action = "new-session"
name = "my-session"

[[step]]
action = "send-keys"
keys = "echo hello\r"
pane = 1

[[step]]
action = "wait-for"
pattern = "hello"
timeout = 5000

[[step]]
action = "wait-for"
pattern = "flaky_output"
retry = 3

[[step]]
action = "assert-screen"
contains = "hello"

[[step]]
action = "assert-screen"
contains = "optional"
continue_on_error = true
```

### TOML Example

Equivalent to the DSL example in [Example 1](#example-1-basic-echo--assert):

```toml
[playbook]
name = "echo-test"
viewport = { cols = 80, rows = 24 }
shell = "sh"

[[step]]
action = "new-session"

[[step]]
action = "send-keys"
keys = "echo hello_world\r"

[[step]]
action = "wait-for"
pattern = "hello_world"

[[step]]
action = "assert-screen"
contains = "hello_world"
```

---

## Sandbox Environment

### How It Works

`bmux playbook run` (without `--target-server`) creates an ephemeral sandbox:

1. Creates a temp directory (`/tmp/bpb-<hex>`) with isolated config, runtime,
   data, and state subdirectories.
2. Writes a minimal `bmux.toml` config with shell and plugin overrides.
3. Spawns a `bmux server start` process pointing at the temp directories.
4. Waits for the server to accept connections (up to 15 seconds).
5. Executes all playbook steps against the sandbox.
6. Stops the server and cleans up the temp directory.

### Plugin Configuration

By default, all bundled plugins are available. Use `@plugin` to control this:

```
@plugin disable=bmux.windows        # disable a specific plugin
@plugin enable=bmux.permissions     # only enable specific plugins
```

When any `enable` is specified, all other plugins are implicitly disabled.

---

## Assertions and Synchronization

### Best Practices for Deterministic Assertions

1. **Always use `wait-for` before `assert-screen`.** Output arrives
   asynchronously -- without a sync barrier, assertions may check stale screen
   content.

2. **Match on distinctive output, not prompts.** Shell prompts vary across
   machines and shells. Match on your command's output instead:

   ```
   send-keys keys='echo UNIQUE_MARKER_123\r'
   wait-for pattern='UNIQUE_MARKER_123'
   ```

3. **Use `\d+` for non-deterministic numbers.** PIDs, line counts, timestamps:

   ```
   wait-for pattern='process started, pid=\d+'
   ```

4. **Use `@env-mode clean` for maximum determinism.** This prevents the
   sandbox from inheriting unpredictable environment variables.

5. **Use `@shell sh` for portable playbooks.** `sh` behavior is more
   predictable across systems than bash/zsh.

6. **Prefer `contains` over `matches` when possible.** Substring matching is
   simpler and less fragile than regex.

---

<div id="interactive-mode-protocol"></div>

## Interactive Mode Protocol

Interactive mode provides a socket-based REPL for LLM agents to control bmux
dynamically.

### Startup

```sh
bmux playbook interactive --viewport 80x24
```

On startup, bmux prints a JSON ready message to stdout:

```json
{
  "status": "ready",
  "socket": "/tmp/bpb-xxx/r/playbook.sock",
  "sandbox_root": "/tmp/bpb-xxx"
}
```

The LLM agent connects to the socket path and communicates via line-delimited
JSON.

### Wire Protocol

Interactive mode is JSON-op only in v2: one JSON object per line (`\n`-delimited).

JSON op examples:

```json
{"op":"hello","protocol_version":1,"client":"llm-agent"}
{"op":"command","request_id":"r1","dsl":"new-session"}
{"op":"subscribe","event_types":["pane_output","cursor_delta","screen_delta"],"screen_delta_format":"line_ops"}
```

**Response:** one JSON object per `\n`.

### Response Schema

```json
{
  "type": "response" | "event" | "error",
  "seq": 1,
  "mono_ns": 1000000,
  "request_id": "optional-correlation-id",
  "status": "ok" | "fail" | "error",
  "action": "send-keys",
  "elapsed_ms": 12,
  "detail": "optional detail string",
  "error": "error message on failure",
  "snapshot": { "id": "...", "panes": [...] },
  "panes": [{ "index": 1, "focused": true, "screen_text": "...", "cursor_row": 0, "cursor_col": 5 }],
  "session_id": "uuid-string",
  "pane_count": 2,
  "focused_pane": 1
}
```

All fields except `status` are optional and omitted when not applicable.

| Field          | Present when                    | Type                           |
| -------------- | ------------------------------- | ------------------------------ |
| `status`       | always                          | `"ok"`, `"fail"`, or `"error"` |
| `action`       | action executed                 | string                         |
| `elapsed_ms`   | action executed                 | u64                            |
| `detail`       | action has detail output        | string                         |
| `error`        | status is `"fail"` or `"error"` | string                         |
| `snapshot`     | `snapshot` action executed      | object                         |
| `panes`        | `screen` command executed       | array of PaneCapture           |
| `session_id`   | `status` command executed       | UUID string                    |
| `pane_count`   | `status` command executed       | u32                            |
| `focused_pane` | `status` command executed       | u32                            |
| `type`         | always                          | message class (`response`, `event`, `error`) |
| `seq`          | always                          | monotonic message sequence number |
| `mono_ns`      | always                          | monotonic nanoseconds since interactive session start |
| `request_id`   | JSON `command`/op requests      | correlation id echoed in response |

### Special Commands

| Op                | Description |
| ----------------- | ----------- |
| `hello`           | Optional capability handshake. |
| `command`         | Execute one DSL action line via `dsl` field (for example `new-session`, `send-keys`, `assert-screen`). |
| `status`          | Return session metadata (`session_id`, `pane_count`, `focused_pane`). |
| `hydrate`         | Hydrate detailed data (`screen_full`, `event_window`, `incident`). |
| `subscribe`       | Start live event streaming with filters and budgets. |
| `unsubscribe`     | Stop live event streaming. |
| `set_watchpoint`  | Register anomaly watchpoint (`kind: "event_burst"`). |
| `clear_watchpoint`| Remove a watchpoint by id. |
| `quit`            | End the interactive session. |

### Push Output Events

After sending `subscribe`, the server pushes events as they arrive.

Pane output event:

```json
{
  "type": "event",
  "status": "ok",
  "event_type": "pane_output",
  "pane_index": 1,
  "output_data": "hello world\n"
}
```

Cursor delta event:

```json
{
  "type": "event",
  "status": "ok",
  "event_type": "cursor_delta",
  "cursor_delta": {
    "pane_index": 1,
    "from": { "row": 10, "col": 1 },
    "to": { "row": 10, "col": 12 },
    "distance": 11
  }
}
```

Screen delta event (LLM-friendly line ops):

```json
{
  "type": "event",
  "status": "ok",
  "event_type": "screen_delta",
  "screen_delta": {
    "pane_index": 1,
    "format": "line_ops",
    "base_hash": "9f1b2c3d4e5f6a70",
    "new_hash": "4f8e1d3ab2c04910",
    "ops": [
      { "op": "set_line", "row": 12, "text": "fn main() {" },
      { "op": "cursor", "row": 12, "col": 11 }
    ]
  }
}
```

Screen delta event (human-readable unified diff):

```json
{
  "type": "event",
  "status": "ok",
  "event_type": "screen_delta",
  "screen_delta": {
    "pane_index": 1,
    "format": "unified_diff",
    "base_hash": "9f1b2c3d4e5f6a70",
    "new_hash": "4f8e1d3ab2c04910",
    "diff": "@@ -13,1 +13,1 @@\n-fn mian() {\n+fn main() {\n"
  }
}
```

Push events have `event_type` set (e.g. `"output"`), which distinguishes them
from command responses. They may arrive between commands or interleaved with
command responses.

| Field         | Type   | Description                                               |
| ------------- | ------ | --------------------------------------------------------- |
| `event_type`  | string | Push event type (`pane_output`, `pane_input`, `cursor_delta`, `screen_delta`, `server_event`, `request_lifecycle`, `watchpoint_hit`) |
| `pane_index`  | u32    | The pane that produced the output                         |
| `output_data` | string | The new output text (UTF-8, may contain escape sequences) |

Watchpoint hit event:

```json
{
  "type": "event",
  "status": "ok",
  "event_type": "watchpoint_hit",
  "watchpoint_hit": {
    "id": "cursor-delta-burst-1",
    "kind": "event_burst",
    "watch_event_type": "cursor_delta",
    "pane_index": 1,
    "summary": "event burst detected: event_type=cursor_delta hits=3 min_hits=3 pane=1",
    "window_ms": 500,
    "min_hits": 3,
    "observed_hits": 3,
    "peak_distance": 12,
    "evidence_seq_start": 42,
    "evidence_seq_end": 42
  }
}
```

`subscribe` JSON options:

- `event_types`: array of event names (`pane_output`, `cursor_delta`, `screen_delta`, `watchpoint_hit`).
- `pane_indexes`: optional pane-index filter.
- `screen_delta_format`: `line_ops`, `unified_diff`, or `auto`.
  - `auto` resolves to `line_ops` for machine-readable clients (e.g. `client: "llm-agent"`) and `unified_diff` otherwise.
- `max_events_per_sec`: optional streaming event budget.
- `max_bytes_per_sec`: optional streaming byte budget.
- `coalesce_ms`: optional per-event-type coalescing interval.

`set_watchpoint` JSON options:

- `id`: required watchpoint id.
- `kind`: `event_burst`.
- `event_type`: required watched stream event (`pane_output`, `pane_input`, `cursor_delta`, `screen_delta`, `server_event`, `request_lifecycle`).
- `pane_index`: optional pane scope (defaults to any pane).
- `window_ms`: burst window in milliseconds (default `500`).
- `min_hits`: required hit count inside `window_ms` (default `3`).
- `contains_regex`: optional regex predicate (v1: supported for `event_type: "pane_output"` only).

Example (only trigger on pane output that matches):

```json
{"op":"set_watchpoint","id":"errors-only","kind":"event_burst","event_type":"pane_output","contains_regex":"(?i)error|panic","min_hits":1,"window_ms":1000}
```

`watchpoint_hit` cannot be watched in v1 (recursive watchpoint loops are blocked).

`hydrate` JSON options:

- `kind: "screen_full"` for full pane snapshot.
- `kind: "event_window"` with `start_seq` and `end_seq`.
- `kind: "incident"` with `id` (watchpoint id) or `around_seq`, plus optional `window_radius`.

Use `unsubscribe` to stop receiving push events.

### Example Session

```
→ new-session
← {"status":"ok","action":"new-session","elapsed_ms":150,"detail":"session_id=a1b2c3..."}

→ send-keys keys='echo hello\r'
← {"status":"ok","action":"send-keys","elapsed_ms":5}

→ screen
← {"status":"ok","action":"screen","panes":[{"index":1,"focused":true,"screen_text":"$ echo hello\nhello\n$ ","cursor_row":2,"cursor_col":2}]}

→ assert-screen contains='hello'
← {"status":"ok","action":"assert-screen","elapsed_ms":10}

→ quit
← {"status":"ok","action":"quit"}
```

---

<div id="recording-to-playbook-conversion"></div>

## Recording to Playbook Conversion

`bmux playbook from-recording` converts a recorded bmux session into a
runnable playbook.

### What Gets Generated

| Element         | Source                                   | How                                                                                                             |
| --------------- | ---------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `new-session`   | `NewSession` request in recording        | Direct mapping                                                                                                  |
| `split-pane`    | `SplitPane` request                      | Direct mapping with direction                                                                                   |
| `focus-pane`    | `FocusPane` request                      | Direct mapping with target index                                                                                |
| `send-keys`     | `AttachInput` / `PaneDirectInput` events | Consecutive inputs within 100ms are coalesced. `pane=N` added when input targets a non-focused pane.            |
| `wait-for`      | `PaneOutputRaw` events after a command   | Last non-empty line of vt100-parsed output becomes the barrier pattern. Digit sequences are collapsed to `\d+`. |
| `assert-screen` | `PaneOutputRaw` events                   | Up to 3 distinctive content lines per response window become `contains=` checks.                                |
| `sleep`         | Gaps > 200ms with no input/output        | Mapped to `sleep ms=N`                                                                                          |
| `@viewport`     | First `AttachSetViewport` request        | Emitted as a directive                                                                                          |

### Pattern Robustness

Generated patterns are made robust to non-deterministic content:

- **Digit sequences** (`12345`) are replaced with `\d+`
- **Regex metacharacters** (`.`, `*`, `+`, `$`, etc.) are escaped
- **Structural text** (command names, paths, error messages) is preserved as
  literal matches

### Limitations

- Multi-client recordings produce playbooks from a single client's perspective.
- Very long outputs (>256KB ring buffer) may have incomplete screen
  reconstruction.
- Some manual editing may be needed for complex workflows (e.g., interactive
  programs, timing-sensitive sequences).

---

## JSON Output Schema

When using `--json`, `bmux playbook run` outputs a `PlaybookResult`:

### `PlaybookResult`

```json
{
  "playbook_name": "my-test",
  "pass": true,
  "steps": [ ... ],
  "snapshots": [ ... ],
  "recording_id": "uuid-string",
  "recording_path": "/path/to/recording",
  "total_elapsed_ms": 1234,
  "error": "top-level error message"
}
```

| Field              | Type              | Always present | Description                             |
| ------------------ | ----------------- | -------------- | --------------------------------------- |
| `playbook_name`    | string \| null    | yes            | From `@name` directive                  |
| `pass`             | bool              | yes            | `true` if all steps passed              |
| `steps`            | StepResult[]      | yes            | Per-step results                        |
| `snapshots`        | SnapshotCapture[] | yes            | Captured snapshots (may be empty)       |
| `recording_id`     | string \| null    | no             | Recording UUID if recording was enabled |
| `recording_path`   | string \| null    | no             | Path to recording directory             |
| `total_elapsed_ms` | u64               | yes            | Wall-clock execution time               |
| `error`            | string \| null    | no             | Top-level error (sandbox failure, etc.) |
| `sandbox_root`     | string \| null    | no             | Sandbox temp dir path (only on failure, for inspection) |

### `StepResult`

```json
{
  "index": 0,
  "action": "send-keys",
  "status": "pass",
  "elapsed_ms": 5,
  "detail": "optional detail"
}
```

On failure, additional structured fields are included:

```json
{
  "index": 3,
  "action": "assert-screen",
  "status": "fail",
  "elapsed_ms": 12,
  "detail": "assert-screen: pane 1 does not contain 'expected_output'",
  "expected": "expected_output",
  "actual": "$ echo something_else\nsomething_else\n$ ",
  "failure_captures": [
    {
      "index": 1,
      "focused": true,
      "screen_text": "$ echo something_else\nsomething_else\n$ ",
      "cursor_row": 2,
      "cursor_col": 2
    }
  ]
}
```

| Field              | Type                  | Description                                                                |
| ------------------ | --------------------- | -------------------------------------------------------------------------- |
| `index`            | u64                   | Step index (0-based)                                                       |
| `action`           | string                | Action name                                                                |
| `status`           | string                | `"pass"`, `"fail"`, or `"skip"`                                            |
| `elapsed_ms`       | u64                   | Step execution time                                                        |
| `detail`           | string \| null        | Action-specific detail. For failures, a human-readable error message.      |
| `expected`         | string \| null        | The expected value/pattern for assertion failures. Only present on `fail`. |
| `actual`           | string \| null        | The actual value/screen text found. Only present on `fail`.                |
| `failure_captures` | PaneCapture[] \| null | Screen capture of all panes at time of failure. Only present on `fail`.    |

The `expected` and `actual` fields allow machine consumers (LLMs) to compare
expected vs actual values without parsing the `detail` string. The
`failure_captures` array provides the full screen state of every pane at the
moment of failure, regardless of which pane was being asserted on.

### `SnapshotCapture`

```json
{
  "id": "after_echo",
  "panes": [ ... ]
}
```

### `PaneCapture`

```json
{
  "index": 1,
  "focused": true,
  "screen_text": "$ echo hello\nhello\n$ ",
  "cursor_row": 2,
  "cursor_col": 2
}
```

| Field         | Type   | Description                                        |
| ------------- | ------ | -------------------------------------------------- |
| `index`       | u32    | Pane index (1-based)                               |
| `focused`     | bool   | Whether this pane has focus                        |
| `screen_text` | string | Visible text, trailing whitespace trimmed per line |
| `cursor_row`  | u16    | Cursor row (0-based)                               |
| `cursor_col`  | u16    | Cursor column (0-based)                            |

---

## Examples

<div id="example-1-basic-echo--assert"></div>

### Example 1: Basic echo + assert

The simplest useful playbook: run a command, wait for output, verify it.

```
@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo hello_world\r'
wait-for pattern='hello_world'
assert-screen contains='hello_world'
```

### Example 2: Multi-pane workflow

Split the terminal, send different commands to each pane, verify both.

```
@viewport cols=120 rows=40
@shell sh
new-session
split-pane direction=vertical
send-keys keys='echo left_pane\r' pane=1
sleep ms=500
assert-screen contains='left_pane' pane=1
send-keys keys='echo right_pane\r' pane=2
sleep ms=500
assert-screen contains='right_pane' pane=2
```

### Example 3: Regex wait-for patterns

Use regex to match output with non-deterministic content.

```
@shell sh
new-session
send-keys keys='echo "pid=$$, count=42"\r'
wait-for pattern='pid=\d+, count=\d+'
```

### Example 4: Clean environment for determinism

Use `@env-mode clean` to ensure the sandbox has a predictable environment.

```
@viewport cols=80 rows=24
@shell sh
@env-mode clean
new-session
send-keys keys='echo $TERM\r'
wait-for pattern='xterm-256color'
assert-screen contains='xterm-256color'
```

### Example 5: Variables and environment overrides

Use `@var` for playbook-scoped constants and `@env` for process environment.

```
@shell sh
@var MARKER=unique_test_id_987
@env MY_APP_MODE=testing
new-session
send-keys keys='echo ${MARKER} $MY_APP_MODE\r'
wait-for pattern='${MARKER}'
assert-screen contains='unique_test_id_987 testing'
```

### Example 6: Snapshot inspection

Capture a named snapshot and inspect its content in the JSON output.

```
@shell sh
new-session
send-keys keys='ls /etc\r'
wait-for pattern='\$'
snapshot id=etc_listing
```

Run with `--json` and inspect `result.snapshots[0].panes[0].screen_text` to
see the directory listing.

### Example 7: Screen and status for debugging

Use `screen` and `status` to inspect state mid-playbook. Useful when developing
a playbook to understand what the terminal shows.

```
@shell sh
new-session
send-keys keys='echo step1\r'
sleep ms=300
screen
status
send-keys keys='echo step2\r'
sleep ms=300
screen
```

Each `screen` step's detail in the JSON output contains the full pane text at
that point in execution.

### Example 8: Expected failure testing

Verify that a specific error condition is detected.

```
@shell sh
new-session
send-keys keys='echo real_output\r'
wait-for pattern='real_output'
assert-screen contains='nonexistent_string'
```

This playbook is expected to fail. Run with `--json` and check
`result.pass == false` and the failing step's `detail` field for the actual
screen content.

### Example 9: Recording conversion workflow

1. Record a session:

   ```sh
   bmux recording start
   # ... do things in bmux ...
   bmux recording stop
   ```

2. Convert to a playbook:

   ```sh
   bmux playbook from-recording <recording-id> --output repro.dsl
   ```

3. Review and edit the generated playbook. The auto-generated `wait-for`
   patterns may need adjustment for your environment.

4. Run it:
   ```sh
   bmux playbook run repro.dsl --json
   ```

### Example 10: CLI variable overrides

Pass variables from the command line to override `@var` defaults:

```sh
# The playbook uses ${MARKER} which defaults to "test"
bmux playbook run test.dsl --var MARKER=production_check --json
```

### Example 11: Retry flaky operations

Use `retry=` on `wait-for` for operations that may not succeed immediately:

```
@shell sh
new-session
send-keys keys='./flaky_server.sh &\r'
wait-for pattern='server ready' timeout=3000 retry=3
```

### Example 12: Continue on error for diagnostics

Use `!continue` to check multiple conditions and report all failures:

```
@shell sh
new-session
send-keys keys='run_diagnostics\r'
wait-for pattern='\$'
assert-screen contains='check_1_ok' !continue
assert-screen contains='check_2_ok' !continue
assert-screen contains='check_3_ok' !continue
snapshot id=diagnostic_results
```

### Example 13: Literal variable references

Use `$${...}` to send literal `${...}` to the terminal:

```
@shell sh
new-session
send-keys keys='echo $${HOME}\r'
wait-for pattern='\$\{HOME\}'
```

### Example 14: LLM-generated playbook pattern

An LLM generating a playbook from a bug description should follow this pattern:

```
# 1. Set up a deterministic environment
@viewport cols=80 rows=24
@shell sh
@env-mode clean

# 2. Create a session
new-session

# 3. For each command:
#    a. send-keys with \r to execute
#    b. wait-for on distinctive output (not the prompt)
#    c. assert-screen to verify expected behavior

send-keys keys='mkdir -p /tmp/test_dir\r'
wait-for pattern='\$'
send-keys keys='ls /tmp/test_dir\r'
wait-for pattern='\$'

# 4. Assert the expected outcome
assert-screen not_contains='No such file'

# 5. Use snapshot for evidence capture
snapshot id=final_state
```

Key principles:

- Always use `@env-mode clean` and `@shell sh` for reproducibility.
- Always `wait-for` after `send-keys` before asserting.
- Match on command output, not shell prompts.
- Use `\d+` in patterns for numbers that may vary.
- Capture a snapshot at the end for debugging if the playbook fails.
