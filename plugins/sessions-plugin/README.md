# bmux_sessions_plugin

Shipped sessions plugin for bmux. Owns the typed session lifecycle
surface: listing sessions, creating them, killing them, and selecting
them. Dependents consume this plugin via \[`bmux_sessions_plugin_api`\].

This plugin is bundled into the `bmux` CLI by default via the
`bundled-plugin-sessions` feature.
