# bmux_connections_plugin_api

Typed contract of the bmux connections plugin, generated from
`bpdl/connections-plugin.bpdl`:

- `hosting-commands` and `hosting-admin-state` (`owner-local`): enable or
  disable remote hosting, pairing sessions, and paired-device administration.
- `hosting-state` (`observe`): whether hosting is enabled, and where.
- `connections-commands` (`owner-local`): invoke a typed service on a selected
  endpoint (`local` or a saved host).

Runtime state, transport and pairing belong in `bmux_connections_plugin`.
