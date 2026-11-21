# bmux_simvar_tui

A terminal interface for watching and replaying
[simvar](https://github.com/MoosicBox/MoosicBox/tree/master/packages/simvar)
simulations, built on bmux's TUI crates. Register its observer with simvar and
run the dashboard:

- **Runs**: every run's seed, progress and result, with each failure's report
  and replay command.
- **Scheduler**: the selected run's simulated threads (state, what each is
  blocked on) and latest scheduling decisions, from `SchedulerView` telemetry.
- **Panels** a simulation adds through `SimPanel`, for its own telemetry
  (the bmux_pty simulator adds its processes and terminals).

With a `ReplayControl`, space pauses and resumes a run at scheduling decisions,
`n` steps one decision and `N` a hundred. Pausing stops only real time, so the
run is the same either way.

It depends on `simvar_observer` and bmux's TUI crates only, never on simvar or
switchy, so it cannot enable simulator features in bmux's workspace.
