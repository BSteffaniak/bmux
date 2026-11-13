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
* Each process starts with default signal handling (whatever the caller ignores, such as
  interrupts, is reset) and inherits no descriptors beyond standard input, output and error.
* Each process starts its own session. `interrupt` (like `ctrl+c`) and `kill` signal its whole
  process group, so commands a shell started stop too, and they still reach what it started after
  it exits while that keeps the terminal open. Dropping a running `PtyProcess` kills it.
* `resize` changes the terminal size and notifies the process.
* `writer` returns a cloneable `PtyWriter` that writes to the terminal while the process runs, as
  typing would: replies to terminal queries, or a user's keystrokes. Writes are queued and carried
  out on a thread of their own.
* `capabilities` says what the platform supports (a real terminal, resizing, writing, signals).
* `control` returns a cloneable `PtyControl` for another thread than the one reading events: it
  resizes (and knows the current `size`), signals, stops output, makes writers, and reports the
  `exit_status` as soon as the process is reaped, before its remaining output is delivered.

## Events

* `next_event` waits; `try_event` and `ready_events(limit)` do not (use them from a UI tick);
  `next_event_timeout` waits at most a duration; `events` iterates until the process exits.
* `Exited` is always the last event. Output still arriving after the process exits is delivered
  first, for up to `drain_grace` (250 ms unless set): descendants that outlive the process can keep
  its terminal open and writing. `PtyExit::output_complete` says whether everything arrived.
* `stop_output` stops delivering output (it is still drained, so the process cannot block on it)
  and reports the exit without waiting out the grace, for a caller whose deadline passed.

## Terminal pairs

For a caller that supervises processes itself, `PtyPair::open(size)` returns both ends of a new
terminal and runs nothing in the background:

```rust
use bmux_pty::{PtyPair, PtySize, SpawnCommand};

let PtyPair { slave, master } = PtyPair::open(PtySize::new(120, 30))?;
let mut command = SpawnCommand::new("zsh");
command.env("TERM", "xterm-256color").cwd(root);
let mut child = slave.spawn(&command)?;
drop(slave); // only the processes keep the terminal open now
let reader = master.try_clone_reader()?; // blocking reads; end of file once nothing holds it
let writer = master.take_writer()?; // once
master.resize(PtySize::new(100, 40))?;
let exit = child.wait()?;
```

* `SpawnCommand` starts from the caller's environment, captured when it is created (on Unix with
  `SHELL` from the user database when unset; on Windows with the system and user environment
  registered for new sessions applied over it, user `PATH` appended). Every change can be read
  back: `argv`, `get_env`, `get_cwd`, `environment`, `utf8_environment`.
* `PtySlave::spawn` looks the program up on the command's `PATH`, runs in its directory (or the
  home directory when that does not exist), and on Unix starts a new session with the terminal
  as its controlling terminal, default signal handling and no stray descriptors.
* `PtyChild` waits (`try_wait`, `wait`), and `kill` ends the process: on Unix `SIGHUP` as a closing
  terminal sends, then `SIGKILL` if it has not exited shortly after. `killer` returns a cloneable
  `PtyChildKiller` for other threads. Killing a process that has exited succeeds.
* On Unix, `PtyMaster` also gives the slave device's path (`tty_name`) for handing the terminal to
  another process, the `foreground_process_group`, and its descriptor. Dropping the writer types a
  newline and the terminal's end-of-file character.
* On Windows the console asks for the cursor position (`CSI 6 n`) before its first process runs,
  and waits until the reader of the master answers (`CSI row ; col R`). The console stays open
  until the master and slave are both dropped; reads then end. A `conpty.dll` shipped beside the
  executable (with its `OpenConsole.exe`) is used in place of the system console host.

## Platforms

Unix systems use `openpty`. Windows uses a pseudo-console (ConPTY): programs see
a console, and `resize` and `writer` work; each process runs in a Job Object, so
`kill` reaches everything it started. A console's `ctrl+c` cannot be raised from
outside it, so on Windows `interrupt` stops the process tree as `kill` does.
With
`capture_stdout` or `input`, which a console cannot redirect separately, and on
other platforms, processes run on plain pipes: output is still streamed, but
programs see no terminal, `resize` has no effect and `writer` is unsupported.
`capabilities` reports which applies to each process.

Whether descendants can keep a terminal open after the process exits differs:
Linux keeps it open
while anything inherited it; macOS ends it with the process's session.

Interpreting the stream (for example with `bmux_terminal_grid`) and presenting
it (for example
with `bmux_tui_components::terminal_view`) are left to the caller.
