# bmux_control_catalog_plugin_api

Typed public API of the bmux control-catalog plugin. The control-catalog
plugin aggregates cross-cutting state from the sessions, contexts, and
clients plugins into a single snapshot, and tracks a monotonic revision
counter that ticks whenever any of those domains change.

Other plugins and attach-side callers depend on this crate for typed
access to catalog queries and events.
