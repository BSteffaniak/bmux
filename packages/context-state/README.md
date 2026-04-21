# bmux_context_state

Neutral primitive crate hosting the reader/writer traits + registry
handle for the contexts-plugin domain. Both `packages/server` (core)
and plugin implementations depend on this crate.

The concrete `ContextState` struct lives in the plugin impl crate
(`bmux_contexts_plugin`); this crate hosts only the trait abstractions
plus a `DefaultNoOp` impl used when the contexts plugin is absent.
