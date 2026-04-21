# bmux_performance_plugin_api

Typed public API of the bmux performance plugin. Other plugins,
`packages/server`, and attach-side callers depend on this crate for
typed access to performance-settings queries, mutations, and events.

Hosts:

- `PerformanceCaptureSettings` — normalized settings record used by
  server's event-push pump and the plugin.
- `PerformanceEventRateLimiter` — sliding-window rate limiter that
  gates performance-recording event emission.
- `PerformanceSettingsHandle` — newtype wrapper for registering the
  settings handle into `bmux_plugin::PluginStateRegistry`.
- Typed `PerformanceRequest` / `PerformanceResponse` wire enums the
  plugin's `performance-commands::dispatch` service routes over.
- Typed `PerformanceEvent` variant the plugin emits on the event bus,
  which the server's event bridge translates to the legacy wire
  `Event::PerformanceSettingsUpdated`.
