# Driving Terminal Programs (for Agents)

`bmux drive` lets a coding agent (or a script) use a terminal program the way a
person tests one by hand: start it, press keys, type, click, resize, and read
what it shows after every action. It is built for the feedback loop agents need
when they write or fix TUIs, interactive CLIs, REPLs and shells.

Agents commonly do this with `tmux send-keys`, `sleep` and `tmux capture-pane`.
That loop guesses at timing, sees only plain text, and cannot tell a program
that is done from one still working or one that crashed. `bmux drive` replaces
each guess with something bmux knows because it is the terminal:

| Agents need to know | tmux loop | `bmux drive` |
| --- | --- | --- |
| When the program has finished reacting | `sleep N` and hope | Waits until no output and no CPU use in the program's foreground process group, or exit |
| Whether it is waiting for input | Unknown | Says when it is quiet but sleeping on a timer, a pipe or the network |
| What changed | Diff it yourself | Changed rows marked `*`, restyled rows (a moved selection) marked `~` |
| What is selected or highlighted | Lost (plain text) | Styled spans: inverse, backgrounds, colors, bold, listed by position |
| Terminal modes | Unknown | Alternate screen, mouse reporting, cursor visibility, title |
| Whether it exited, and how | Check separately | Every report says `EXITED` with exit code or signal |
| Whether it broke the terminal | Invisible | Protocol lint: modes left on at exit, unended synchronized updates, flicker, invalid UTF-8 |
| What scrolled away | `capture-pane -S` | `look --history N`, logical lines rebuilt from the full output |
| Colors and layout | No | `screenshot` writes a PNG |
| A regression test of what was tried | Write one by hand | Every action is recorded as a replayable script, runnable at several sizes |

## Quick start

```bmux-cli
bmux drive start --size 100x30 -- htop
bmux drive press Down Down
bmux drive press F10
bmux drive stop
```

Each command prints a report such as:

```text
[menu] press k k -> settled after 262ms | wrote 344 B in 2 writes, 2 full clears
screen 50x14 | cursor row 9 col 1 (hidden) | alternate screen | mouse press-release
~ 4|  Save file
~ 6|  Quit
styled (differs from default):
  row 4 col 1-22: "Save file" [inverse]
```

Rows and columns are numbered from 1, and `click ROW COL` uses the same
numbers. With a single session running, no session name is needed; otherwise
pass `-s NAME`.

## How it works

`bmux drive start` creates a private bmux server for the session (under
`$TMPDIR/bmux-drive-$UID`, or `BMUX_DRIVE_HOME`) and launches the program as
that server's pane program with your exact environment, current directory and
`TERM=xterm-256color`. Sessions persist between commands: each `bmux drive`
invocation is a short client call, so agents can use it from any shell tool.

Input goes to the program the way a terminal would send it. Keys honor the
program's cursor-key mode, each key of a `press` is a separate write with a
short settle between keys (as typed), mouse events use the encoding the program
enabled, and paste uses bracketed paste when the program asked for it. A click
on a program that never enabled mouse reporting is not sent, and the report
says so.

After input, `bmux drive` waits until the program **settles**: no output and,
on Linux, no CPU use by any process in the terminal's foreground process group
for 200 ms (`--settle-ms`), or exit. A build that prints nothing while it
compiles is still busy; a program blocked reading its terminal is idle as soon
as it stops drawing. When the program is quiet but blocked on a timer, a pipe,
the network or a file lock, the report says it is not waiting for input.

The screen, cursor and modes come from the pane runtime's own terminal state;
protocol lint and `look --history` replay the program's complete ordered output
capture, so nothing it wrote is missed.

The `drive` command belongs to the `bmux.playbook` plugin and uses the pane
runtime's typed execution, presentation and state services; nothing about it is
special-cased in core.

## Commands

| Command | What it does |
| --- | --- |
| `start [--size COLSxROWS] [--cwd DIR] [--env K=V] [--name N] -- PROGRAM ARGS...` | Start a program and show its first settled screen |
| `press KEY...` | Keys: `Enter Esc Tab S-Tab Backspace Space Up Down Left Right Home End PgUp PgDn Insert Delete F1`-`F12`, characters, `C-x`, `M-x`, `S-x`, repeats like `Down*5` |
| `type TEXT [--enter]` | Type text; real newlines are Enter |
| `paste TEXT` | Paste, bracketed when the program enabled it |
| `click ROW COL [--button right] [--double]` | Click a cell |
| `scroll up\|down [N] [--at ROW COL]` | Mouse wheel |
| `drag R1 C1 R2 C2` | Drag with the left button |
| `resize COLSxROWS` | Resize (SIGWINCH) |
| `look [--history [N]]` | Show the screen, or the last N lines of output including scrolled-off ones |
| `wait --text RE \| --gone RE \| --idle \| --exit \| --change [--timeout SECS]` | Wait for an outcome; fails with the screen when it never comes |
| `expect --text S \| --no-text S \| --cursor R C \| --running \| --exited \| --exit-code N \| --styled TEXT --style STYLE` | Check now; exits 1 with the reason on failure |
| `screenshot [PATH]` | PNG of the terminal |
| `lint` | Terminal protocol problems (see below) |
| `stop [--grace SECS]` | Stop the program and session; prints exit status, final screen, lint and script |
| `list` | Running sessions |
| `script [-o FILE]` | The session's actions as a replayable script |
| `run FILE [--size 80x24,40x12]` | Replay a script, once per size; exits 1 if any run fails |
| `watch` | Mirror a session read-only in your own terminal |
| `mcp` | Serve the same tools over the Model Context Protocol |
| `guide` | Print the agent guide (usable as a skill or `AGENTS.md` section) |

Every command accepts `--json`, `--changes` (only changed rows), `--no-screen`
(summary only), `--styles` (every styled span) and `--settle-ms MS`. Exit codes
are 0 for success, 1 for a failed check (`expect`, `wait`, `run`) and 2 for an
error.

## Protocol lint

`lint`, `stop`, and every action (for new problems) report what the program did
to the terminal:

- **Left on at exit** (errors): alternate screen, mouse reporting, bracketed
  paste, focus reporting, hidden cursor, application cursor keys or keypad,
  kitty keyboard flags, a scrolling region, text attributes. These break the
  user's shell after the program exits. They are checked once the program has
  exited, so quit it with its own keys before `stop` to test the normal exit
  path; `stop` on a running program tests the signal path instead.
- **Synchronized update never ended** (error): `CSI ?2026h` without the
  matching `l` freezes real terminals.
- **Flicker** (warning): repeated whole-screen clears without synchronized
  updates.
- **Invalid UTF-8** (warning), with the first offending bytes.
- **Full terminal resets** (warning), clipboard reads and bells (info).
- **Terminal queries sent**, such as DA1, cursor position, colors, XTVERSION,
  DECRQM and XTGETTCAP: worth knowing when a program hangs at startup on a
  terminal that does not answer.

## Scripts and size matrices

Every action is appended to the session's script, along with every passing
`wait --text` and `expect`. Save it with `bmux drive script -o
tests/menu.drive`:

```text
start --size 50x14 -- ./menu.py
press j j
expect --styled Settings --style inverse
click 6 5
expect --styled Quit --style inverse
press k k Enter
expect --text 'chose: Save file'
```

`bmux drive run tests/menu.drive --size 80x24,40x12,200x60` replays it at each
size. Each run reports pass or fail, the failing line with the screen at that
moment, and the lint findings, so layout bugs at small or large sizes show up
as test failures.

## MCP

```sh
claude mcp add bmux-drive -- bmux drive mcp
```

The server offers `start`, `type`, `press`, `paste`, `click`, `scroll`, `drag`,
`resize`, `look`, `wait`, `expect`, `screenshot` (returned as an image), `lint`,
`stop`, `list`, `script` and `run_script`. Tool calls build the same command
lines as the CLI and share its sessions, so a session started over MCP can be
inspected with the CLI and the other way round.

## Limits

- CPU-based settling and wait classification read `/proc` and work on Linux.
  Elsewhere, settling relies on output alone; use `wait --text` for programs
  that work silently.
- Programs that animate (spinners, clocks) never settle; actions report them as
  still busy after their timeout (`--timeout`). Wait for the text you expect
  instead.
- Do not attach to a drive session with `bmux attach`. A regular client resizes
  the pane outside the execution's control path, which ends the execution. Use
  `bmux drive watch`.
- Every `bmux` plugin command snapshots and hashes the bmux executable at
  startup. With multi-gigabyte debug builds this costs seconds per command;
  release builds take a fraction of a second. The MCP server pays it once.
