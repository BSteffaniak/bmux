# bmux_tui_runtime

Bounded, domain-neutral scheduling and presentation runtime for terminal user interfaces built with `bmux_tui`.

The crate owns event admission, fair scheduling, commands, timers, redraw coalescing, render cadence, terminal input lifecycle, shutdown, and neutral runtime statistics. Application state and product behavior remain with the consumer.

See [`../../docs/tui-runtime.md`](../../docs/tui-runtime.md) for the architecture contract.
