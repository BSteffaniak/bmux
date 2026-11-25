# bmux_clients_plugin

Shipped clients plugin for bmux. Tracks per-client identity, selected
session, and follow state. Dependents consume this plugin via
\[`bmux_clients_plugin_api`\].

A global follower (`follow CLIENT --global`) stays on its leader's tab: when
the leader's current tab changes (contexts announces
`session-active-context-changed` naming the leader), every global follower is
selected onto the same tab for itself (contexts `select-for-client`), so its
attach moves exactly as for its own switch.

From a key binding or the command palette, `follow` and `unfollow` act for
that attachment. From a command line they act for the invoking principal's
attached terminal (`clients-follow-v1`): the one named with `--client <id>`,
or else its only attachment. With several and no `--client`, or with none,
the command fails and says so; a command line never redirects another
principal's attachment.

This plugin is bundled into the `bmux` CLI by default via the
`bundled-plugin-clients` feature.
