# Fixes the simulations found

Bugs found by bmux's deterministic simulations (`simulation/`) and fixed: fleets, guided
and explored runs, determinism checks, and the conformance suites built alongside them.

From 2026-11-22, such a fix carries trailers in its commit message (see AGENTS.md), so new
ones are listed by git rather than here:

```bash
git log --format='%h %ad %s' --date=short --grep='^Found-by: simulation'
```

This page records the fixes made before that convention. Each fix's replay seed, where it
has one, is in the simulation's `regression-seeds`.

## Product bugs in bmux

Bugs a real bmux could hit.

| Date | Commit | Area | Bug |
| --- | --- | --- | --- |
| 2026-11-21 | `03ef66c9` | `bmux_pty` | Killing or interrupting a pane could signal a newer process that reused the exited leader's id: the leader was reaped before it was marked exited. |
| 2026-11-21 | `d40fcc22` | pane runtime | A late sweep by session id (a restarted pane's background shutdown, say) could reach a new session that reused the id. The Unix backends now check the leader's start identity first. |
| 2026-11-21 | `8a27e583` | pane runtime | A released Windows job was swept again when its leader exited. |
| 2026-11-22 | `cc0ba173` | pane runtime | Every restart reported the running pane exited: the replaced program's exit notice named the pane. |
| 2026-11-22 | `a6371884` | pane runtime | A program that exited before its session was adopted left its children running. On Linux, with the terminal still held, opening the pane hung. |
| 2026-11-22 | `e908331d` | pane runtime | With `sweep_on_exit` off, a stopped pane could release the jobs it was meant to end; a pane refused before its commit always did. |
| 2026-11-22 | `c73f3a10` | pane runtime | A pane whose waiter thread failed to start never noticed its process exit, and closing it hung. (The first fix with `Found-by` trailers.) |

## Determinism leaks in bmux

Code that was harmless natively but made simulated runs depend on something a seed does not
control, so failures could not be replayed.

| Date | Commit | Area | Leak |
| --- | --- | --- | --- |
| 2026-11-21 | `7e8c8b14` | pane runtime | Grid snapshot acknowledgements locked outputs in address order; caches were process-wide, shared by parallel runs. |
| 2026-11-22 | `327a89f0` | pane runtime | Sessions were named by `Session::new`'s random v4 id, which the scan could not see. |
| 2026-11-22 | `f86480de` | plugin registry | Plugin state initializers ran under a std write lock; one reaching a switch point could hang a run polled on several workers. |
| 2026-11-22 | `16034f14` | pane runtime | Crash watching used tokio's `watch`, which wakes waiters in no seeded order. |
| 2026-11-22 | `bde86244` | pane runtime | The durable-state writer named its staging file with the host's process id (found by the fresh-process determinism check). |

## Bugs in the simulators (MoosicBox)

Found while building or running the simulations; fixed in switchy and simvar.

| Date | Commit | Crate | Bug |
| --- | --- | --- | --- |
| 2026-11-21 | `5d1bb1e00` | switchy_pty | A simulated process killed by a signal could still start children, so a group kill could miss them. |
| 2026-11-21 | `3a707dcea` | switchy_pty | A terminal kept pointing at its session after the leader exited. |
| 2026-11-21 | `e4dfef512` | switchy_pty | Under concurrent opens, macOS `openpty` can fail with XNU's internal `EREDRIVEOPEN` (-6). Found by the conformance suite; a real macOS behavior. |
| 2026-11-21 | `d85632a47` | switchy_time | A replay with `SIMULATOR_EPOCH_OFFSET` skipped a draw, so multi-task replays took different paths. |
| 2026-11-21 | `b0332ff1f` | switchy_async | `select!` chose branches with an unseeded per-thread generator. |
| 2026-11-21 | `f8970dcf3` | simvar | Results were kept per worker, so run counts were wrong. |
| 2026-11-22 | `9ebdbe139` | switchy_async | `block_on` polled its future with a no-op waker and slept through its own wake-up. |
| 2026-11-22 | `973b0c836` | switchy_async | Spawning a task did not wake an idle executor. |
| 2026-11-22 | `465cd18aa` | switchy_async | Spawning onto one simulator runtime from another's task panicked. |
| 2026-11-22 | `3720647bb` | switchy_async | The simulator had no `watch` of its own; tokio's wakes in no seeded order. |
| 2026-11-22 | `8fa16f682` | switchy_thread | A killed thread whose destructor waited on a lock held by another victim was parked forever. |
| 2026-11-22 | `a85accea9`, `b49f4f3bc` | switchy_pty | A simulated console echoed typing before its cursor report, and typed withheld bytes without the line discipline. |
| 2026-11-22 | `2a6efdc49` | switchy_pty | Closing a simulated console spared processes ignoring hang-ups; Windows ends them all. |
| 2026-11-22 | `ee00d0349` | switchy_pty | A process whose spawning thread was killed (a server crash) between registering it and starting its program ran forever; program threads also died with their spawner's thread group. |
| 2026-11-22 | `5cf1b7ba8` | switchy_async | A shut-down runtime's tasks kept being polled on other threads. |
| 2026-11-22 | `30ff1967d` | switchy_async | The spawn failure fault hit the simulator's own threads. |
| 2026-11-22 | `8bf6f416a` | simvar | A backward wall clock jump crashed the fleet worker timing a run. |
| 2026-11-22 | `3f7328eeb` | simvar | A panic outside a run's panic catching ended a fleet silently as "0 runs, 0 failed". |

## Open

- **A close hang with `sweep_on_exit` off.** A pane whose shell exited, leaving a job
  attached to its terminal, after restarts, can fail to close; crash recovery can also leave
  sessions running. See the [bug-hunting plan](bug-hunting-simulation-plan.md#open).
