# bmux_simvar_tui

The simulation dashboard for
[simvar](https://github.com/MoosicBox/MoosicBox/tree/master/packages/simvar)
simulations, built on bmux's TUI crates. It runs a fleet of simulations
continuously and travels in time through any one run. Run it through a
simulation: `cargo sim pane-runtime --tui`.

## Fleet

- **Workers**: each worker's latest outcomes as a strip (green passed, red
  failed), its run count, and the run it is making. Click one (or pick it with
  `↑↓` and press `enter`) to open its run.
- **Throughput**: runs per second, scheduling decisions per second, and how
  much faster than real time the runs go.
- **Outcomes**: passed and failed, split by the shapes runs report (each
  personality, for example). Click one to show only those runs.
- **Coverage**: distinct thread schedules among finished runs, and how many
  runs showed each shape (`crash`, `restart`, …).
- **Invariants exercised**: how often the fleet checked each invariant;
  rarely or never checked ones are flagged.
- **Failures**: kept, not fatal. They are grouped by what failed, replayed
  to confirm them, shrunk in the background (`⇲`), announced in a toast, and
  saved under `target/sim-findings/<simulation>/` (or `$SIMULATOR_FINDINGS`),
  which outlives the session. What a replay says:
  - `↺✓ reproduced`: it failed the same way.
  - `↺✗ nondeterministic`: it passed on the same build, so the seed ran
    differently.
  - `◷ earlier build`: saved by another build. Seeds replay only on the build
    that ran them, so `↺` checks it on this one: `↺✓ still fails` or
    `✔ passes now` (fixed, or the code changed).

  The Failures tab shows each group with what failed, its replay command, and
  buttons to open, reproduce, shrink and forget it (`enter`, `c`, `s`, `x`).
  Opening a failure replays its run up to the failure, beside why it failed.

`p` pauses or resumes the fleet, and `+`/`-` change how many runs go at once.

## A run

A run's tab shows the run, live or replayed:

- **Timeline**: one lane per simulated thread, darker where it ran more, with
  the simulation's events (opens, crashes, failures, …) below. Click or drag
  on it to go to a decision, click an event to jump there, and click a lane to
  open that thread.
- **Transport**: start, previous event, -100, -1, play/pause, +1, +100, next
  event, end, and a scrubber over the whole run. Keys: `space`, `←→` (`⇧` for
  a hundred), `[ ]`, `home`/`end`.
- **Pages**: Overview, Threads (each thread's state and the decisions it was
  in), Invariants, Decisions, then the simulation's own views (`SimView`):
  lists of entities that show a card when clicked and open a full page when
  clicked again (the kernel's processes and terminals, the pane runtime's
  panes).

Going back in time replays the run's seed in the simulation's process. A few
replays are kept parked at keyframes along the run, so going anywhere costs
at most one keyframe's worth of decisions (`timetravel`). The fleet keeps
running meanwhile. Breakpoints: `b` (the next decision), or commands such as
`break 1200`, `break fail` and `break crash` (pause at that kind of event).

## Everywhere

Everything responds to the pointer: click to select and show a card, click again to open, and scroll the
list under the pointer. `:` or `ctrl+k` opens a fuzzy command palette
(`goto N`, `back N`, `parallel N`, …), `esc` goes back, `t` switches between
dark and light, and `?` shows every key.

## Embedding

A simulation registers `observer()` with simvar, emits `telemetry` (`Pulse`,
`RunIndex`, `Invariants`, `Shape`, its own data for its `SimView`s), implements
`Controller` over simvar's fleet control, and runs `Dashboard::new(options,
events).run()`; `sim_support` in bmux's `simulation/` workspace does all of
this. The `store` and `report` modules are the same model without the
dashboard, for headless runs (`cargo sim NAME --fleet`).

It depends on `simvar_observer` and bmux's TUI crates only, never on simvar or
switchy, so it cannot enable simulator features in bmux's workspace.
