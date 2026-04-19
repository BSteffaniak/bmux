# bmux_sessions_plugin_api

Typed public API of the bmux sessions plugin. Consume this crate to
call the sessions plugin's typed services from another plugin or from
the CLI/attach runtime.

The `sessions_state`, `sessions_commands`, and `sessions_events`
modules are generated at compile time from `bpdl/sessions-plugin.bpdl`
via the `bmux_plugin_schema_macros::schema!` macro.
