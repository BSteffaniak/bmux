# bmux_performance_plugin

Shipped performance plugin for bmux. Owns `PerformanceCaptureSettings`
and serves performance settings queries/mutations via typed dispatch
on `performance-commands::dispatch`.

Server writes into the registered settings handle on construction;
attach-side callers read + mutate via `bmux_client::performance_status`
/ `bmux_client::performance_set`, which typed-dispatch through this
plugin. The plugin emits `PerformanceEvent::SettingsUpdated` on the
plugin event bus whenever settings change, and the server's event
bridge translates that to the legacy wire `Event::PerformanceSettingsUpdated`.
