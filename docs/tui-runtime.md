# BMUX TUI runtime

`bmux_tui_runtime` is the domain-neutral execution layer for terminal user interfaces built from `bmux_tui` primitives and, optionally, `bmux_tui_components` controls. It owns scheduling and presentation mechanics; applications retain their state and product behavior.

## Layer ownership

```text
bmux_tui                  terminal primitives, frames, buffers, events, backends
bmux_tui_components       reusable controls and component-local interaction state
bmux_tui_runtime          bounded admission, scheduling, timers, commands, cadence
application/plugin        product state, semantic updates, effects, navigation, policy
```

`bmux_tui_runtime` depends on `bmux_tui`. It does not depend on `bmux_tui_components`, and neither lower layer depends on the runtime. The runtime must not interpret BMUX or consumer product domains such as windows, sessions, panes, clients, contexts, permissions, model turns, or tools.

## Program model

A runtime serially delivers typed events to one `Program` owner. An event is terminal input, an application message, a keyed timer, or a command completion represented by an application message. `Program::update` mutates application-owned state and returns explicit presentation or exit intent. Long-running work is returned as owned `'static` commands rather than awaited in `update`; work that must continue borrowing mutable program state is not a runtime command and must first be split into an owned request/result boundary by the application.

The runtime never owns canonical application state. It owns only bounded queues, task generations, timer deadlines, redraw state, and presentation cadence.

## Admission and ordering

Admission is explicit rather than inferred from message contents.

- **Reliable messages** use a bounded FIFO mailbox. Asynchronous senders wait for capacity; `try_send` reports saturation. Accepted messages are not silently dropped.
- **Terminal events** use a separate bounded FIFO mailbox so application floods cannot consume all input capacity. Reliable key, paste, click, wheel, and focus events use this path.
- **Latest-value messages** are keyed and bounded by key count. Replacing a pending value is explicit, latest-wins, and counted. Applications may use it only when skipped intermediate values are semantically safe.
- **Redraw requests** use a dirty latch. Repeated requests do not consume queue entries.

FIFO is guaranteed within each reliable mailbox. No total ordering is invented across independent producers or between reliable and latest-value mailboxes. Applications that require cross-source ordering must carry and validate their own sequence contract.

The runtime has no generic drop-oldest policy. Lifecycle, authorization, cancellation, and durable semantic updates must use reliable admission unless their owning domain defines a safe replacement contract.

## Fairness and backpressure

Each scheduler turn has both a message-count budget and a wall-time budget. Terminal input is checked before bounded application work, and due timers and presentation are reconsidered between turns. Saturated reliable producers are backpressured by bounded channel capacity. Latest-value producers are bounded by configured key capacity.

Budget exhaustion yields back to the Tokio scheduler and records a statistic. It does not discard accepted work.

## Timers

Timers are keyed one-shot deadlines. Scheduling the same key replaces its pending deadline. A delivered timer is removed before `Program::update`; periodic behavior reschedules itself explicitly. This avoids an implicit global tick and allows idle runtimes to sleep until actual work is due.

## Commands and cancellation

Commands are application-supplied futures that return zero or one application message. The runtime supports concurrent commands and keyed start-if-idle, replace, queue-latest, and cancel policies. Keyed generations prevent stale completions from replaced work from entering the application mailbox.

Runtime task abortion is local lifecycle management only. It must not be represented as cancellation of canonical application or remote work unless the application separately performs and observes that domain operation.

## Rendering

Semantic updates never wait for frame cadence. An update marks presentation dirty; the runtime presents the latest application state when the frame deadline permits. Repeated invalidations coalesce. The next deadline is based on completion of the preceding presentation, so a delayed frame does not create catch-up bursts.

Presentation is synchronous in the initial design. This keeps the committed terminal buffer, hit map, cursor, image scene, and application-visible presentation acknowledgement aligned. A future asynchronous presenter requires an owned frame snapshot and explicit commit acknowledgement; it must not be introduced merely to move a blocking write to another task.

A presenter failure terminates the runtime without treating the failed frame as committed. A reset request invalidates the terminal backend before the next presentation.

## Managed terminal input

With the `crossterm` feature, the runtime can own a blocking terminal reader thread. It forwards events through bounded terminal admission. Dropping the input source requests shutdown and detaches a reader blocked in the operating-system backend; the thread exits after the next event or when runtime admission closes. The runtime does not claim that a blocking OS read is synchronously cancellable.

## Shutdown and errors

Graceful exit stops admission processing, cancels runtime-owned command and subscription tasks, performs no speculative final semantic update, and returns the final program and runtime statistics to the caller. Immediate abort follows the same ownership cleanup but may skip an application-requested final redraw. Program and presenter errors are surfaced distinctly.

## Observability

A lightweight runtime statistics snapshot reports queue depth and high-water marks, backpressure/rejection counts, latest-value replacements, processed event classes, scheduler budget exhaustion, redraw requests and coalescing, frame count and scheduling delay, command lifecycle, stale completions, and timer activity. Consumers translate these neutral measurements into their own metrics systems.

## Non-goals

- Product state, navigation semantics, authorization, persistence, network clients, and plugin behavior.
- Portable frontend or plugin protocol contracts.
- Interpreting event payloads to decide whether they may be dropped or replaced.
- Making local task cancellation authoritative for remote work.
- Replacing `bmux_tui` frame construction or retained-buffer terminal diffing.
- Mechanically reproducing Bubble Tea APIs.
