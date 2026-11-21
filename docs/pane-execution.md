# Pane processes

The `bmux.pane_runtime` plugin starts each pane's program directly on the pane's
terminal. There is no helper process per pane: a pane costs one process, the
program itself.

## Process lifetime

Each pane's program leads its own terminal session (Unix) or runs in its own
Job Object (Windows). bmux cleans up everything the program started in that
session:

- when the pane is closed or its program is stopped;
- when the bmux server shuts down, panics or aborts;
- on Windows, also when the server is killed outright: closing the Job Object
  ends every process in it.

If the server is killed outright on Unix (`SIGKILL`, out-of-memory), the kernel
hangs up every pane terminal at once, which ends ordinary programs and jobs.
Jobs that ignore the hangup (`nohup`, disowned jobs) keep running until the next
server for the same runtime starts; it sweeps the sessions recorded under
`<runtime_dir>/pane-sessions/` before starting any pane. Managed program
executions left unfinished by such a crash are marked as ended with their exit
status lost.

Processes that leave the pane's session on purpose (`setsid`) are never touched.

## Configuration

```toml
[plugins.settings."bmux.pane_runtime".execution]
# Kill background and nohup jobs when a pane's shell exits normally (default).
# Set to false to leave them running after the shell exits, as tmux does.
sweep_on_exit = true
```

`sweep_on_exit` only affects a normal exit of an ordinary pane's shell; managed
program executions always sweep. With `sweep_on_exit = false`, jobs left running
when the shell exits are handled by whether they still use the pane's terminal:

- Jobs that still hold the terminal stay attached to the pane. Their output is
  shown, and closing the pane (or server shutdown) cleans them up.
- Once the pane's terminal reaches end of file (no job holds it any more), the
  remaining jobs are released and keep running; closing the pane no longer
  affects them. On macOS the kernel can detach the terminal from the session
  as soon as its shell exits, so leftover jobs are often released right away.
