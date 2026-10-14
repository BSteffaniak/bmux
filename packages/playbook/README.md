# Playbook domain library

`bmux_playbook` owns the existing playbook DSL/TOML parsers, include resolution,
execution models, variable substitution, escaping and result comparison. The CLI
executor and managed playbook plugin consume this one implementation.

This is explicitly a **product domain library**, not neutral core infrastructure.
Moving models here does not authorize importing them into server/client/IPC,
plugin SDK or other generic host layers. It has no dependency on CLI, plugin
implementation, plugin API, attach runtime or compositor internals.

## Ownership

- `types`: playbook configuration, actions and result models.
- `parse_dsl` / `parse_toml`: existing input formats; both produce the same model.
- `parser`: file/include resolution and bounded immutable source snapshots for
  explicitly trusted managed execution. Unix snapshots open each path component
  relative to an owned root descriptor and reject symlinks and escapes.
- `subst`: parse/runtime variable substitution.
- `diff`: structured comparison of execution results.

Executable-deck permission checks, process lifecycle, generation ownership and
service registration remain in the execution plugin. Signal and interactive
terminal acquisition remain CLI adapters. The public BPDL lifecycle contract
remains in `bmux_playbook_plugin_api`; this crate does not replace it with a
handwritten transport layer.

Original parser/model/substitution/diff tests move with their source files.
Source snapshot tests cover frozen includes and symlink/escape rejection.
Validation is coordinator-owned for the current implementation lane; no test
execution is claimed by this extraction.
