---
name: bmux-drive
description: Test and debug terminal programs (TUIs, interactive CLIs, REPLs, shells) by running them in a private terminal and driving them with keys, text, mouse and resizes, seeing the settled screen after every action. Use instead of tmux send-keys/capture-pane, expect scripts or sleep loops whenever you need to see what a terminal program actually shows or check that a change to one works.
---

# Driving terminal programs with `bmux drive`

`bmux drive` runs a program in a private terminal and lets you use it the way a
person would, with feedback after every action. Each action waits until the
program has **settled**: no output and no CPU use for a moment, or the program
exited. You never need `sleep`. Then it shows you:

- the screen, rows numbered from 1, changed rows marked `*`;
- cells styled differently from the rest: selection bars, focus, errors in red;
- cursor position, alternate screen, mouse mode, window title;
- what the program wrote to draw the change (bytes, full clears);
- whether the program **EXITED**, with its exit code or signal;
- new terminal protocol problems (see `lint`).

Sessions persist between commands. With a single session running, `-s NAME` is
never needed.

## The loop

```sh
bmux drive start -- ./target/debug/my-tui --flag   # first settled screen
bmux drive press Down Down Enter                   # act, read the new screen
bmux drive type 'hello world' --enter
bmux drive wait --text 'Saved|Error' --timeout 60  # slow work: wait for an outcome
bmux drive expect --styled 'Save' --style inverse  # assert; exits 1 on failure
bmux drive press q                                 # quit the way a user would
bmux drive stop                                    # exit status, final screen, lint, script
```

Read the report after every action before choosing the next one. If a report
says `still busy`, the program kept drawing or computing (a spinner, a clock, a
build): use `wait --text` for the result you expect rather than repeating
actions.

## Commands

- `bmux drive start [--size 80x24] [--cwd DIR] [--env K=V] [--name N] -- PROGRAM ARGS...`
  starts a program. It inherits your environment (PATH, HOME, ...), with
  `TERM=xterm-256color`. Relative program paths resolve against `--cwd`.
- `bmux drive press KEY...` sends keys: `Enter Esc Tab S-Tab Backspace Space Up
  Down Left Right Home End PgUp PgDn Insert Delete F1`-`F12`, single characters
  (`q`, `Q`, `/`, `:`), `C-c` (ctrl), `M-x` (alt), `S-Up` (shift), `C-M-Left`,
  repeats such as `Down*5`. Arrow keys follow the program's cursor-key mode.
- `bmux drive type TEXT [--enter]` types text literally; real newlines are Enter.
- `bmux drive paste TEXT` pastes (bracketed paste when the program enabled it).
- `bmux drive click ROW COL [--button right] [--double]`,
  `bmux drive scroll up|down [N] [--at ROW COL]`,
  `bmux drive drag R1 C1 R2 C2` use the mouse, with rows and columns as
  numbered in the screen. If the program never enabled mouse reporting, nothing
  is sent and the report says so: a real terminal would not deliver it either.
- `bmux drive resize 40x12` resizes (SIGWINCH). Check layouts at small and large
  sizes; the report notes when the screen did not take the new size.
- `bmux drive look` shows the screen without acting; `look --history [N]` prints
  the last N lines of output (default 200), including what scrolled off the
  screen, such as a long test run's beginning.
- `bmux drive wait --text REGEX | --gone REGEX | --idle | --exit | --change [--timeout SECS]`
  waits for an outcome (default 30 s) and fails with the screen if it never
  comes.
- `bmux drive expect --text S | --no-text S | --cursor ROW COL | --running |
  --exited | --exit-code N | --styled TEXT --style 'inverse'` checks the screen
  now and exits 1 with the reason when it fails. Style words are those reports
  print: `inverse`, `bold`, `underline`, `fg=red`, `bg=#303030`.
- `bmux drive screenshot [PATH]` writes a PNG of the terminal. Read it when color,
  alignment or box drawing matters.
- `bmux drive lint` reports what the program did to the terminal. When it has
  exited: alternate screen, mouse reporting, bracketed paste, hidden cursor,
  keypad or kitty keyboard modes, scrolling regions or colors left on (they
  break the user's shell). Always: synchronized updates never ended, whole-screen
  clears that flicker, invalid UTF-8, full resets, and the terminal queries it
  sent.
- `bmux drive stop [--grace SECS]` ends the session: SIGHUP, then kill if
  needed. It reports the exit status, the final screen, a full lint and the
  recorded script. Quit with the program's own keys first when the exit path
  matters.
- `bmux drive list` lists sessions; `-s NAME` picks one when several run.
- `bmux drive script [-o FILE]` prints the session as a replayable script: every
  action, plus every `wait --text` and `expect` that passed.
- `bmux drive run FILE [--size 80x24,40x12,200x60]` replays a script, once per
  size, and exits 1 if any run fails, showing the failing line, the screen and
  lint findings. Commit scripts as regression tests.
- `bmux drive watch` mirrors a session live in a human's terminal, read-only:
  it sends nothing to the program, so it never disturbs what you are doing.
- `bmux drive mcp` serves all of this as MCP tools over stdio
  (`claude mcp add bmux-drive -- bmux drive mcp`).
- `bmux drive guide` prints this guide.

Every command takes `--json` for structured output, `--changes` to print only
changed rows, `--no-screen` for the summary line only, and `--styles` to list
every styled span.

## Testing a change to a TUI

1. Build, then `start` the program at the size users run it.
2. Exercise the feature with `press`/`type`/`click`, reading every report.
3. Add `expect` checks for what must be true; they go into the script.
4. `resize 40x12` and back to check layout, wrapping and truncation.
5. Quit with the program's own keys, then `stop` and read the lint: an exit that
   leaves modes on is a bug.
6. Save `bmux drive script -o tests/feature.drive`, and run it with
   `bmux drive run tests/feature.drive --size 80x24,40x12` after later changes.
