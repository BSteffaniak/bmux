# bmux_simvar_tui

A terminal interface for watching and replaying
[simvar](https://github.com/MoosicBox/MoosicBox/tree/master/packages/simvar)
simulations, built on bmux's TUI crates. Register its observer with simvar and
run the dashboard:

- **Runs**: every run's seed, progress and result, with each failure's report
  and replay command.
- **Scheduler**: the selected run's simulated threads (state, what each is
  blocked on), latest scheduling decisions and decision rate, from
  `SchedulerView` telemetry, and each invariant's status from `Invariants`
  telemetry.
- **Panels** a simulation adds through `SimPanel`, for its own telemetry
  (the bmux_pty simulator adds its processes and terminals).

With a `ReplayControl`, space pauses and resumes a run at scheduling decisions,
`n` steps one decision and `N` a hundred, and `b` and `B` go back one and a
hundred. `:` opens a command line: `goto N`, `back N`, `break decision N`,
`break step N`, `break block THREAD [ON]`, `break fail` and `clear`. Going back
ends the dashboard with `Exit::Seek`; `seek` then reruns the program as that
run's seed, paused at the decision (`replay_control` reads where to pause from
`SIMVAR_TUI_PAUSE_AT`). Pausing stops only real time, so the run is the same
either way.

It depends on `simvar_observer` and bmux's TUI crates only, never on simvar or
switchy, so it cannot enable simulator features in bmux's workspace.
