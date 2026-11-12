# bmux_pty

Run a command in a pseudo-terminal and stream its output.

Programs run through `bmux_pty` see a real terminal, so they keep their colors,
progress
displays and line editing. Output arrives as events in the order it was read,
through a bounded
queue: a consumer that falls behind slows the process down instead of losing
output or buffering
without limit.

```rust
use bmux_pty::{PtyCommand, PtyEvent, PtySize};

let mut process = PtyCommand::new("git")
    .args(["push", "--progress"])
    .current_dir(root)
    .capture_stdout() // optional: stdout stays a byte-exact pipe; stderr is the terminal
    .spawn(PtySize::new(120, 30))?;

for event in process.events() {
    match event {
        PtyEvent::Output(bytes) => { /* a terminal stream: feed it to a terminal emulator */ }
        PtyEvent::Stdout(bytes) => { /* piped stdout, with `capture_stdout` */ }
        PtyEvent::Exited(exit) => println!("exited: {:?}", exit.code()),
    }
}
```

## Running processes

* `PtyCommand` is built like `std::process::Command`: `arg`, `args`, `current_dir`, `env`,
  `env_remove`. `TERM` is `xterm-256color` unless set, since the reader of the stream decides
  which terminal it emulates.
* `capture_stdout` keeps standard output on a pipe so it can be parsed exactly, while standard
  error (where most tools write progress and diagnostics) stays a terminal. Such a process has no
  controlling terminal, so it cannot prompt through `/dev/tty`.
* `input` feeds bytes to standard input through a pipe, then closes it.
* Each process starts its own session. `interrupt` (like `ctrl+c`) and `kill` signal its whole
  process group, so commands a shell started stop too, and they still reach what it started after
  it exits while that keeps the terminal open. Dropping a running `PtyProcess` kills it.
* `resize` changes the terminal size and notifies the process.
* `writer` returns a cloneable `PtyWriter` that writes to the terminal while the process runs, as
  typing would: replies to terminal queries, or a user's keystrokes. Writes are queued and carried
  out on a thread of their own.
* `capabilities` says what the platform supports (a real terminal, resizing, writing, signals).

## Events

* `next_event` waits; `try_event` and `ready_events(limit)` do not (use them from a UI tick);
  `next_event_timeout` waits at most a duration; `events` iterates until the process exits.
* `Exited` is always the last event. Output still arriving after the process exits is delivered
  first, for up to `drain_grace` (250 ms unless set): descendants that outlive the process can keep
  its terminal open and writing. `PtyExit::output_complete` says whether everything arrived.
* `stop_output` stops delivering output (it is still drained, so the process cannot block on it)
  and reports the exit without waiting out the grace, for a caller whose deadline passed.

## Platforms

Unix systems use `openpty`. Elsewhere processes run on plain pipes: output is
still streamed, but
programs see no terminal, `resize` has no effect and `writer` is unsupported.
`capabilities`
reports which applies.

Whether descendants can keep a terminal open after the process exits differs:
Linux keeps it open
while anything inherited it; macOS ends it with the process's session.

Interpreting the stream (for example with `bmux_terminal_grid`) and presenting
it (for example
with `bmux_tui_components::terminal_view`) are left to the caller.
