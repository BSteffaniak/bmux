# bmux_performance_state

Neutral primitive crate hosting the `PerformanceSettingsReader`/`Writer`
traits + registry handle for the performance-plugin domain. Both
`packages/server` (event-push rate limiter hot-path) and the
performance plugin depend on this crate.

The `PerformanceCaptureSettings` concrete record also lives here
because server's event-push pump reads it on the hot path and the
record shape is stable.
