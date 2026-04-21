# bmux_control_catalog_plugin

Control-catalog plugin for bmux. Aggregates cross-cutting session,
context, and client state into a single snapshot with a monotonic
revision counter. Subscribes to `SessionEvent`, `ContextEvent`, and
`ClientEvent` on the plugin event bus and ticks its revision whenever
any of those domains change.

Attach UIs query the catalog via the typed `control-catalog-state` BPDL
surface.
