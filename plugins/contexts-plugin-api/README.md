# bmux_contexts_plugin_api

Typed public API of the bmux contexts plugin. Contexts are a
higher-level grouping layer over sessions. Consume this crate to call
the contexts plugin's typed services from another plugin or from the
CLI/attach runtime.

The `contexts_state`, `contexts_commands`, and `contexts_events`
modules are generated at compile time from `bpdl/contexts-plugin.bpdl`
via the `bmux_plugin_schema_macros::schema!` macro.
