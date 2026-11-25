# bmux_connections_plugin_api

Typed contract of the bmux connections plugin, generated from
`bpdl/connections-plugin.bpdl`:

- `hosting-commands` and `hosting-admin-state` (`owner-local`): enable or
  disable remote hosting, pairing sessions, paired-device administration,
  live connections and the audit log (`audit-log`).
- `hosting-state` (`observe`): whether hosting is enabled and why, where it
  listens, and its relay (connected, latency).
- `connections-commands` (`owner-local`): invoke a typed service on a selected
  endpoint (`local` or a saved host).
- `saved-hosts-state` (`owner-local`): the hosts this machine has paired with,
  which other plugins (such as `bmux.cluster`) name as endpoints.

Runtime state, transport and pairing belong in `bmux_connections_plugin`.
