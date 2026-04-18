# BMUX Plugin Definition Language (BPDL)

BPDL is the typed interface description language for BMUX plugins. It lets
plugin authors declare the shapes (records, variants, enums) and operations
(queries, commands, events) that define a plugin's public contract, and
provides codegen to produce idiomatic Rust bindings for consumers.

See `docs/bpdl-spec.md` (workspace root) for the full grammar and semantics.
