# bmux_client_state

Neutral primitive crate hosting the reader/writer traits + registry
handle for the clients-plugin domain. Both `packages/server` (core)
and plugin implementations depend on this crate to interact with
follow-state without creating a core→plugin dependency.

The concrete `FollowState` struct lives in the plugin impl crate
(`bmux_clients_plugin`); this crate hosts only the trait abstractions
plus a `DefaultNoOp` impl used when the clients plugin is absent.
