# bmux_clients_plugin_api

Typed public API of the bmux clients plugin. Tracks per-client
identity, selected session, and follow state. Consume this crate to
call the clients plugin's typed services from another plugin or from
the CLI/attach runtime.

The `clients_state`, `clients_commands`, and `clients_events` modules
are generated at compile time from `bpdl/clients-plugin.bpdl` via the
`bmux_plugin_schema_macros::schema!` macro.
