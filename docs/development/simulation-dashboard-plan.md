# Plan: the simulation dashboard

Phase 17 of the [bmux_pty simulation plan](pty-deterministic-simulation-plan.md),
after the [platform-neutral simulation](platform-neutral-simulation-plan.md).

## Goal

One dashboard for every bmux simulation, with two jobs:

1. **Fleet**: run simulations continuously, many at once, for as long as you
   let it. Show the whole fleet at a glance: throughput, what is running, what
   failed, how the failures cluster, and which invariants are being
   exercised. A failure is kept, not fatal, and is already reproduced and
   shrunk by the time you look at it.
2. **Investigate**: open any run, live or failed, and debug it. Watch it,
   pause it, step it, rewind it, scrub along its timeline, break on what
   matters, and drill from the fleet down to one byte a program wrote at one
   scheduling decision.

It should look and feel like a first-class bmux application: themed, colorful
with meaning, animated where that helps, fully usable with the mouse, and
discoverable without reading the docs.

## Where things stand

- **`packages/simvar-tui`** (977 lines) already runs on bmux's TUI crates,
  but uses a sliver of them. The whole screen is one `TextBlock` of styled
  lines repainted every 50 ms. Input is keyboard only, with no hit testing,
  router, panes, tables, trees, scroll views, charts or theme. Simulation
  panels (`SimPanel`) can only return `Vec<Line>`. Going back in time
  re-executes the whole process at the seed (`seek`), which also throws away
  every other run.
- **`simulation/support`** feeds the dashboard. It supplies scheduler, invariant
  and kernel telemetry, and renders terminal screens with
  `terminal_view::TerminalState`.
- **simvar's harness** runs `SIMULATOR_RUNS` runs on `SIMULATOR_MAX_PARALLEL`
  workers. The first failure stops every worker. There is no unbounded mode,
  and nothing can steer it while it runs.
- **dst-demo** uses simvar's stock dashboard: a grid of one progress gauge per
  worker plus a "completed/total" header, with `q` to quit. What is worth
  keeping is the shape (N workers pulling run indices, a fresh seed per run,
  a replay command per failure). The view itself has no aggregates,
  throughput, failure list, history, mouse support or drill-down, and stops
  at the first failure.
- **bmux's TUI stack has what this needs, unused here:**
  - `bmux_tui_runtime` (`Program`, timers, coalesced redraws, `ProgramHarness`
    for tests);
  - `InteractionRouter` (hit testing, hover, activation, focus, gestures with
    drag and double-click);
  - `ComponentTheme::derive` (a full palette from seed colors with enforced
    contrast, quantized to the terminal's color depth);
  - the components: `panel_group` (resizable, collapsible, maximizable
    splits), `pane`, `tab_bar`, `breadcrumbs`, `table`, `tree_view`,
    `virtual_list`, `uniform_list`, `scroll_view`, `chart`, `canvas`
    (sub-cell drawing), `bar_chart`, `sparkline`, `progress_bar`, `badge`,
    `spinner`, `toast_stack`, `menu`, `modal_frame`, `key_hint_bar`,
    `status_bar`, `terminal_view`, `diff_viewer` and `text_input`.

  `examples/tui-components-gallery` and `plugins/presenter-plugin` show the
  idioms end to end.

## Design

### Shape of the application

```
 ┌ bmux sim · pane-runtime ─────────────────────── ● 14 workers ▮▮▮▮▮▮▮▮▮▯ 312 runs/s ┐
 │ Fleet  │ Failures 3 │ Run 4812 ▸ Timeline ▸ Panes ▸ pane 2                      ⌕  │  ← tabs + breadcrumbs
 ├──────────────────────────────────────────────────────────────────────────────────┤
 │                                                                                  │
 │   (the focused view: Fleet, a failure, or a run at some depth)                   │
 │                                                                                  │
 ├──────────────────────────────────────────────────────────────────────────────────┤
 │ ◀◀ ◀ ❚❚ ▶ ▶▶  decision 18 204 / 51 977  ━━━━━━━━━━━━━╋━━━━━━━━━━━━━━━━━━━━━━━━  │  ← transport (in a run)
 │ space pause · ←/→ step · [ ] seek · click to drill · esc back · : command   ? keys│  ← key hints
 └──────────────────────────────────────────────────────────────────────────────────┘
```

- **Navigation is a stack of places**: Fleet → Run → an aspect of the run
  (Timeline, Threads, Invariants, and the simulation's own views such as
  Panes, Kernel or Terminals) → one entity (a thread, a process, a pane, a
  terminal) → its raw data (decision trace, bytes). Clicking drills in, Esc
  or a breadcrumb climbs out, and the stack is shown as `breadcrumbs`.
  Several runs can be open as tabs.
- **Progressive detail.** Every entity has three levels of detail, chosen by
  how much attention it has, not by a mode:
  - in a list it is a **row**: a glyph, a colored state, one number and a
    sparkline;
  - hovering or keyboard-focusing it shows a **card** in a side panel:
    vitals, recent history and its links;
  - opening it gives a **page**: everything, with its own tabs.

  Every view in the plan is specified at these three levels.
- **Everything is clickable.** Rows open, chart points seek, timeline markers
  jump, thread lanes select, badges filter, dividers drag, and the wheel
  scrolls whatever is under the pointer. Every action also has a key, and
  `?` shows them in context. A command palette (`:`, or `ctrl+k`) fuzzy-finds
  actions, runs, seeds, threads and processes.

### Fleet

```
 ┌ Throughput ──────────────────────────┐┌ Outcomes ────────────┐┌ Invariants exercised ─────────┐
 │ runs/s  ▁▂▄▆█▇▇█▆▇█▇▆█▇ 312          ││ ■ 48 211 passed      ││ 1 opens        ███████████ 2.1M│
 │ dec/s   ▃▄▆▇█▇█▇▇▆█▇█▇▇ 41.2M        ││ ■      3 failed  ⚠   ││ 2 input order  █████████▌  1.7M│
 │ sim/real ×1 830                      ││ ■      0 nondet      ││ 6 crash sweep  ██▏        212K │
 └──────────────────────────────────────┘└──────────────────────┘│ 7 restart      ███▍       388K │
 ┌ Workers ─────────────────────────────────────────────────────┐└───────────────────────────────┘
 │ w01 ▕████████████▏ run 48 214  seed 9f3a…  linux   ▂▄▇ 61k/s  │┌ Recent failures ──────────────┐
 │ w02 ▕██████▏       run 48 216  seed 0c71…  windows ▅▇█ 72k/s  ││ ✖ 6 crash sweep  windows 2m ↺✓│
 │ w03 ▕██▏           run 48 219  seed 5512…  macos   ▃▃▅ 40k/s  ││ ✖ 4 cleanup      linux   9m ↺✓│
 │  …                                                            ││   shrunk 51 977 → 212 decisions│
 └───────────────────────────────────────────────────────────────┘└───────────────────────────────┘
```

- **Workers**: a `table` row per worker with a live progress bar, the run,
  seed, personality badge, and a decision-rate sparkline. Clicking a row
  opens that live run. It keeps running, and you can pause it there.
- **Throughput**: runs/s, decisions/s, and the simulated-to-real time ratio,
  as `chart`/`sparkline` over the session.
- **Outcomes**: passed, failed and nondeterministic counts, split by
  personality and other run properties. Clicking a segment filters every
  panel.
- **Invariants exercised**: how often each invariant was checked across the
  fleet. An invariant that is rarely or never checked is flagged, since it
  shows a workload gap.
- **Coverage signals**: the number of distinct decision-trace hashes and of
  distinct "shapes" a simulation reports (for example pane counts, crashes or
  personalities). A plateau means the workload has stopped finding new
  ground.
- **Failures** are kept, not fatal. Each one is grouped by a signature (its
  first violated invariant plus a normalized message). It is automatically
  re-run to confirm it reproduces (✓), and shrunk in the background
  (`SIMULATOR_SHRINK`) to its smallest schedule. The Failures tab lists the
  groups with counts and first/last seen. A toast announces a new one, and a
  click opens it at the failing decision.
- **Steering**: pause or resume the fleet, change parallelism, restrict to a
  personality or property, or pin a seed range, all from the UI.

### Investigate: one run

A run page has a **timeline** across the top and tabs below it.

```
 Timeline  ┊ decisions 0 ──────────────────────────────────────────────── 51 977
 threads   ┊ main      ▆▆▁▁▁▆▆▆▁▁▆▆▁▁▁▁▆▆▆▁▁▁▆▆▆▆▁▁▁▁▆▆▁▁▆▆▆▁▁▆▆▁▁▁▆▆▆▁▁▁▆▆▆▁▁▁
           ┊ executor  ▁▁▆▆▁▁▁▁▆▁▁▁▆▆▆▁▁▁▁▆▆▁▁▁▁▆▆▆▁▁▁▁▆▆▁▁▁▆▁▁▆▆▁▁▁▆▁▁▁▆▆▁▁▁▆▆
           ┊ pane-2-rd ▁▁▁▁▆▁▁▁▁▁▁▁▁▁▁▆▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▆▆▆▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁
 events    ┊ ◆open  ◆open   ⌨ ⌨  ↔resize   ⚡crash  ◆open  ↻restart        ✖ inv 6
           ┊                          ▲ you are here (18 204)
```

- **Swimlanes**: one lane per simulated thread or task, with color intensity
  for how often it ran in each decision bucket. Blocked spans are amber, and
  a dashed segment shows what the thread was waiting on. Zooming with
  ctrl+wheel or dragging a range shows down to single decisions. Clicking a
  lane selects the thread; clicking a cell seeks there.
- **Event track**: the simulation's own events (opens, keystrokes, resizes,
  crashes, signals, spawns, invariant checks and violations) as glyph
  markers. Hovering one shows its card; clicking seeks to it.
- **Transport**: play, pause, step ±1 / ±100, run to the next event of a
  kind, play speed, and drag-to-seek on the scrubber.
- **Tabs**:
  - **Threads**: a `tree_view` of threads with state and what they block on,
    plus each thread's recent decisions and a "why is this blocked" chain.
  - **Invariants**: each invariant's checks over time, with violations and
    the evidence each one recorded.
  - **Decisions**: a virtualized trace with filters and a diff against
    another run.
  - **The simulation's own views**, contributed through `SimView`, below:
    - **Kernel**: process tree, sessions and groups, signals, and terminals,
      each terminal drawn live with `terminal_view` at the current decision,
      with what programs wrote and what was delivered side by side.
    - **Panes**: each pane's grid, size, typed lines and answers, session and
      exit.
    - **Local terminal**: client frames and attach/detach history.

### Time travel

Determinism makes every moment of a run reproducible. The design keeps
rewinding instant enough to feel like scrubbing:

- **Index pass.** The first time a run executes, live or replayed, it records
  a compact timeline:
  - per decision bucket, which threads ran and blocked;
  - every simulation event with its decision number;
  - invariant checks.

  The timeline, swimlanes and event track draw from this index without
  re-executing anything.
- **Replays run in process.** A replay is the run's seed re-executed on its
  own worker, with a `ReplayControl` and no telemetry until it nears its
  target. The fleet keeps running alongside. This replaces `seek`'s
  re-execution of the whole process.
- **Keyframes.** When a run is opened, parked replays are started at evenly
  spaced decisions (8 by default, bounded by memory). Seeking to decision N
  uses the nearest parked replay at or before N and runs it forward, so going
  back costs at most one keyframe interval. Parked replays are cheap: they
  are paused simulated threads. The keyframe used is consumed, and a new one
  refills behind it.
- **Detail at a moment** (a terminal's screen, the process table, a pane's
  grid) comes from the paused replay's live state, so it is exact rather than
  reconstructed.
- **Breakpoints** are set with the mouse or the palette:
  - at a decision;
  - when a thread blocks or runs;
  - at an event kind (for example "the next crash");
  - when an invariant is checked or fails;
  - on a predicate over a simulation's state, through `SimView`.

  They show as markers on the timeline and in a Breakpoints list.
- **Nondeterminism.** When a replay's trace hash diverges from the
  recording, the dashboard shows both traces aligned in `diff_viewer` and
  stops at the first differing decision, with both threads' backtraces. This
  automates the `PANE_RUNTIME_SIM_TRACE` recipe.

### Look and feel

- **Theme**: `ComponentTheme::derive` from a small palette (a dark default
  and a light one, switchable at runtime), quantized automatically to the
  terminal's color depth.
- **Colors carry meaning, applied consistently:**
  - green: running or passed;
  - amber: blocked or waiting;
  - red: failed or violated;
  - violet: replay or time travel ("you are not live");
  - grey: finished or idle.

  Each personality has its own badge color. Each simulated thread gets a
  stable hue (OKLCH, evenly spaced, contrast-checked) used everywhere it
  appears: lane, tree, decision trace, card. Meaning never depends on color
  alone; every state also has a glyph.
- **Motion where it informs:**
  - progress bars fill smoothly;
  - a new failure pulses its row and raises a toast;
  - the "you are here" marker glides on seek;
  - spinners mark background work (shrinking, reproducing, refilling
    keyframes).

  Motion stops when nothing changes, so the dashboard idles at zero CPU.
- **Layout**: `panel_group` splits that you can resize and collapse by
  dragging, and maximize with a double-click on a title. The layout adapts to
  the terminal's width (breakpoints) and is remembered per simulation.

### Extending it: `SimView`

`SimPanel` (`lines(run, width) -> Vec<Line>`) becomes `SimView`. A simulation
contributes typed telemetry and components:

- the **entities** it knows (kind, id, title, state, parent), so they appear
  in the tree, the palette, and breadcrumbs;
- for each entity, a **row**, a **card**, and a **page** (bmux `Element`s);
- the **events** that go on the timeline (kind, glyph, color, decision,
  entity);
- the **predicates** it offers for breakpoints;
- the **coverage shapes** it reports for the Fleet.

`sim_support` implements the Kernel and Terminals views once for every
simulation that uses `switchy_pty`. Each simulation adds only its own
domain, for example `simulation/pane-runtime`'s Panes.

### Runs, control and robustness

- **Harness changes** (simvar, MoosicBox):
  - an unbounded mode (`SIMULATOR_RUNS=forever`);
  - keep going after a failure (`SIMULATOR_ON_FAILURE=continue`), with the
    failure's report kept;
  - a control channel the observer side can use to pause or resume workers,
    change parallelism, and start an in-process replay (seed, target
    decision, `ReplayControl`) or a shrink as a job on a reserved worker;
  - `RunFinished` with real and simulated time, and the worker.
- **UI isolation**: simulation threads never wait on the UI. Telemetry goes
  through bounded queues that drop the oldest samples, except for results,
  failures and the index, which are never dropped. The runtime coalesces
  redraws. A slow terminal slows only the picture.
- **Findings persist.** Each failure group is written to
  `target/sim-findings/<simulation>/` with its seed, epoch offset, build
  identity, replay command, shrunk schedule and signature. Restarting the
  dashboard on the same build reloads them. A new build marks them
  "re-check", and the dashboard can re-run them all, which also produces
  candidates for regression seeds.
- **Headless parity**: the same store drives a plain-text reporter for CI and
  `--no-tui` (periodic summary lines, failures with replay commands), so the
  UI never holds logic the CI cannot see.
- **Terminal safety**: `TerminalGuard` restores the terminal on exit, panic or
  signal, and `ctrl+z` suspends and resumes.
- **Tests**:
  - `ProgramHarness` drives the application deterministically, so every view
    and interaction (click, drag, wheel, keys) has a test;
  - rendered-frame snapshots for each view at standard sizes;
  - a performance budget: a frame for 64 workers and 50 000 decisions renders
    within 8 ms in release mode, checked by a test.

### Entry points

- `cargo sim NAME --tui` opens Fleet with continuous runs.
- `cargo sim NAME --tui SEED` (or `SIMULATOR_SEED=... --tui`) opens that run,
  paused at its start.
- `cargo sim NAME --tui --findings` opens the persisted failures.
- The existing `SIMULATOR_*` variables keep working everywhere.

## Phases

| # | Phase | Repo | Acceptance |
| --- | --- | --- | --- |
| 17.1 | Application shell on `bmux_tui_runtime`: router, theme, tabs and breadcrumbs, `panel_group` layout, key hints, palette, mouse everywhere; today's views ported as components | bmux | harness tests for navigation, click, drag and wheel; existing dashboard features kept |
| 17.2 | Harness: unbounded runs, continue on failure, control channel, in-process replay and shrink jobs | MoosicBox | simvar tests; `cargo sim` unchanged without the flags |
| 17.3 | Store and findings: event store, aggregates, failure grouping, persisted findings, headless reporter | bmux | store unit tests; CI output from the reporter |
| 17.4 | Fleet view: workers, throughput, outcomes, invariants exercised, coverage, failure groups with auto-reproduce and shrink | bmux | runs for hours at full parallelism with flat memory; failures appear reproduced and shrunk |
| 17.5 | Run view and progressive detail: index pass, timeline with swimlanes and events, Threads, Invariants, Decisions; row, card and page for each entity | bmux | snapshot tests; drill from Fleet to a decision by mouse alone |
| 17.6 | Time travel: keyframe replays, scrubbing, step back without re-execution, breakpoint UI, nondeterminism diff | both | seeking back anywhere in a 50 000-decision run within 1 s; replay matches recording |
| 17.7 | `SimView` and domain views: Kernel and Terminals in `sim_support`; Panes; Local terminal; predicates and coverage shapes | bmux | all three simulations use it; `SimPanel` removed |
| 17.8 | Polish: motion, light theme, adaptive layouts, remembered layout, performance budget, docs and a screencast-style walkthrough in the README | bmux | performance test; manual review on three terminals (truecolor, 256, 16) |

17.1 and 17.2 can proceed in parallel, as can 17.3 and the early part of
17.5. Each phase ships a usable dashboard; the old one is removed in 17.1.

## Risks

- **Memory in long fleet sessions.** Keep only aggregates and bounded recent
  history for passing runs. Failures and their indexes are small and kept.
- **Keyframe cost.** Parked replays hold the run's simulated threads and
  kernel. Cap them by count and estimated size, and fall back to
  re-executing from the nearest keyframe.
- **Telemetry overhead in fleet mode.** Only the index and results are always
  on; detailed telemetry flows only for runs someone has open.
- **Terminal variety.** Mouse modes, truecolor and glyph widths differ.
  Capability detection already quantizes colors; glyph fallbacks (ASCII
  bars, no braille) are kept for 16-color and non-UTF-8 terminals.

## Implementation notes

Built on 2026-11-21:

- **17.2 (MoosicBox):** `simvar::control()` steers a running simulation:
  - pause and resume;
  - `set_max_parallel` (workers grow and retire);
  - `stop`;
  - in-process `replay` jobs (seed, epoch offset and an optional
    `ReplayControl`) and `shrink` jobs.

  The rest:
  - `SIMULATOR_RUNS=forever` and `SIMULATOR_ON_FAILURE=continue`.
  - `current_run()` and `current_replay()`, so a run carries its own control
    instead of sharing one.
  - Richer events: `RunKind`, worker, epoch offset, real and simulated time, and
    `Shrink` progress.
  - Bugs found while testing it: a step-count underflow on early stops, a
    worker-retirement race, and panics outside runs being swallowed.
- **17.1, 17.3 to 17.8 (bmux):** `bmux_simvar_tui` was rewritten:
  - `store` and `report`: the model and its headless view.
  - `findings`: saved failures.
  - `theme`: a dark and a light palette with meanings, and per-thread hues.
  - `ui`: painting through bmux components (panes, sparklines, progress bars,
    key hints) with hit regions.
  - `timetravel`: keyframe replays.
  - The application itself: actions shared by pointer, keys and the palette;
    tabs, breadcrumb-like stacks, toasts and the palette.

  `sim_support` gives every simulation:
  - a per-run timeline index and heartbeat;
  - full telemetry only for watched runs and replays;
  - `event`, `shape` and `finish_run`;
  - the Kernel `SimView`;
  - `FleetController`;
  - `main` (`--tui [SEED[:EPOCH]]`, `--fleet`).

  The pane runtime simulation adds a Panes view, events and shapes.
- **Seen working:** in a real terminal (driven headlessly), the fleet ran at
  about 70 runs a second on 6 workers, opening a run and stepping back through
  parked keyframes worked, and with crash recovery disabled the dashboard
  grouped, reproduced, shrank (to "any schedule") and saved the failures,
  only on Linux and macOS, as Windows' jobs end with the server.
- **Differences from the plan:**
  - The timeline's "dashed segment showing what a thread waits on" is the
    thread's state in its lane label and in the Threads page.
  - The nondeterminism diff shows `↺✗` (a replay that passed), not an aligned
    trace diff.
  - Predicates for breakpoints are event kinds (`break crash`), not arbitrary
    state predicates.
  - Dividers are dragged through the dashboard's own drag handles (the fleet's
    columns, the run overview's rows), not `panel_group`, and are not saved
    between sessions.
  - The 8 ms performance budget is not yet a test.
