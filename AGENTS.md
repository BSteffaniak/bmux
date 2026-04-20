# AGENTS.md

This file defines REQUIRED validation steps for coding agents working in this repo.

## Core Architecture Boundary (REQUIRED)

BMUX core must remain domain-agnostic. Windows, sessions, contexts, clients, and permissions are all plugin domains, not core architecture. Core crates provide generic primitives; plugins own product-specific behavior.

### Hard Rules

- Do not add or keep domain logic (windows, sessions, contexts, clients, panes, permissions) in core architecture layers.
- Core architecture includes at least:
  - `packages/server/**`
  - `packages/client/**`
  - `packages/ipc/**`
  - `packages/session/**`
  - `packages/event/**`
  - `packages/plugin-sdk/**` — shared SDK types, identifier newtypes, typed-dispatch primitives
  - `packages/plugin/**` — host-side plugin loader, registry, and runtime traits
  - `packages/plugin-schema/**` + `packages/plugin-schema-macros/**` — BPDL codegen
- Core architecture must provide only generic host primitives (`storage`, `log`, `recording`, `call_service`, `execute_kernel_request`). Core crates MUST NOT host domain convenience helpers — they live in each plugin as private modules or are reached through typed BPDL services.
- In core architecture, avoid domain-specific types/fields/events/APIs for any plugin domain.
- Domain behavior must be implemented through plugins and generic plugin/service invoke paths (`Request::InvokeService` + typed plugin-api crates, or `ServiceCaller::execute_kernel_request` for kernel-level primitives).
- Core defaults when plugins are missing:
  - Missing windows plugin: baseline single terminal attach/session/pane flow still works.
  - Missing permissions plugin: permissive single-user behavior.
  - Missing sessions/contexts/clients plugins: baseline server behavior still works (plugins provide typed-dispatch facades over core state).

### Plugin Power Expectations

- Plugins are first-class and may implement critical product behavior.
- Prefer extending generic plugin APIs/capabilities over adding core-special-case code.
- If a feature seems domain-specific, place it in a plugin unless there is a strong, documented reason it must be core-agnostic runtime plumbing.
- When a plugin needs to reach core kernel state (sessions, contexts, panes), use `ServiceCaller::execute_kernel_request(bmux_ipc::Request::*)` directly. Foundational plugins (sessions, contexts, clients, windows) are allowed to call core IPC this way; other plugins must go through typed BPDL services exposed by the foundational plugins.

### Review Gate Before Finishing (REQUIRED)

For any non-doc code change, verify no forbidden domain leakage was introduced in core architecture:

- Run content checks (or equivalent) to confirm no new core references to windows/permissions/sessions/contexts/clients/panes domain concepts.
- `HostRuntimeApi` must remain domain-agnostic — only `core_cli_command_run_path`, `plugin_command_run`, `storage_get`, `storage_set`, `log_write`, `recording_write_event`.
- Domain convenience helpers belong in plugins (as private modules) or are reached through typed BPDL services, not in `HostRuntimeApi` or any other core crate.
- If any core-side domain leakage is found, treat as blocking and refactor before finishing.

These boundary rules are strict and take precedence over convenience.

## Core Rule

If you change code, run the relevant tests before finishing and report exactly what you ran and whether it passed.

In addition, for any code change anywhere in the repo, run:

- `cargo nextest run --no-fail-fast`

This is required. Treat failures as blocking, and do not finish with known flaky/failing tests.

## Clippy (REQUIRED)

For any code change anywhere in the repo, run:

- `cargo clippy --all-targets -- -D warnings`

This must produce zero errors and zero warnings. Treat any warning as blocking.

Key rules when fixing clippy issues:

- Fix the root cause. Do not add crate-level `#![allow(clippy::...)]` to suppress warnings.
- Item-level `#[allow(clippy::...)]` is acceptable only when:
  - The lint is a genuine false positive for that specific item (e.g., `too_many_lines` on a state machine function that would be less readable if split).
  - The lint cannot be satisfied without breaking the API or reducing clarity (e.g., `cast_possible_truncation` on a bounded numeric conversion in rendering code).
  - A comment explains why the allow is needed.
- Never use `cargo clippy --fix` without verifying it compiles and tests pass afterward -- auto-fixes can break code in macro contexts and async functions.
- Wildcard imports (`use something::*`) are not allowed in production code. They are acceptable in `#[cfg(test)] mod tests` blocks with `use super::*`.

## Cargo Machete (REQUIRED)

For any code change anywhere in the repo, run:

- `cargo machete --with-metadata`

This must report zero unused dependencies. Treat any finding as blocking.

Key rules when addressing unused dependency findings:

- Remove the dependency from `Cargo.toml` if it has no source code references.
- Also remove any feature propagation entries (e.g., `"dep_name/fail-on-warnings"`) for removed deps.
- If a dependency is only used for feature propagation (not imported in source code), remove both the dep and propagation -- transitive deps handle their own features.
- Use `[package.metadata.cargo-machete] ignored = [...]` only for genuine false positives:
  - Dependencies used via proc macro code generation (e.g., `container!` generating `::crate_name::Type` paths).
  - Dev-dependencies that cargo-machete cannot trace into test code.
  - A comment in the `ignored` list explains why the ignore is needed.

## Plugin Changes (REQUIRED)

If a change touches `plugins/**`, rebuild bundled plugins before finishing:

- `bmux plugin rebuild --all-workspace-plugins`

During iteration you may run targeted rebuilds with selectors (plugin id, short name, or crate name), but final validation for plugin changes should run with `--all-workspace-plugins`.

If `bmux plugin rebuild --all-workspace-plugins` is unavailable in the current environment, run direct cargo builds for plugin crates (for example, `cargo build -p <plugin-crate> ...`).

## Minimum Required Commands (CLI/runtime work)

For changes in `packages/cli/**` (runtime, input, pane/layout, protocol, terminal handling), run:

1. `cargo nextest run --no-fail-fast`
2. `cargo test -p bmux_cli`
3. `cargo check -p bmux_cli`
4. `./scripts/smoke-pty-runtime.sh`

## Compatibility-Related Changes (REQUIRED extra check)

If the change touches terminal protocol behavior, TERM/profile logic, or query/reply handling, also run:

- `./scripts/compat-matrix.sh` (command 5 in the full CLI/runtime validation sequence)

This includes edits under:

- `packages/cli/src/runtime/terminal_protocol.rs`
- `packages/cli/src/runtime/pane_runtime.rs`
- `packages/cli/src/runtime/mod.rs` (terminal doctor / profile / term resolution)
- `packages/config/src/lib.rs` (`behavior.pane_term`, trace flags, profile settings)

## Change-to-Tests Mapping

- Protocol/query/reply/TERM/profile changes
  - Run all 5 commands above.
- Input/keymap/runtime command handling changes
  - Run 1-4.
- Layout/pane lifecycle/compositor changes
  - Run 1-4.
- Config-only changes
  - Run at least 1-3; include 4 if behavior affects runtime startup.
- Any plugin changes under `plugins/**`
  - Run `bmux plugin rebuild --all-workspace-plugins`.
- Docs-only changes
  - No mandatory runtime commands.

In addition, for any non-doc code change, always run:

- `cargo nextest run --no-fail-fast`
- `cargo clippy --all-targets -- -D warnings`
- `cargo machete --with-metadata`

## Completion Reporting Format

Agents should report test execution in final response using this format:

- `cargo test -p bmux_cli` - PASS/FAIL
- `cargo check -p bmux_cli` - PASS/FAIL
- `cargo nextest run --no-fail-fast` - PASS/FAIL
- `cargo clippy --all-targets -- -D warnings` - PASS/FAIL
- `cargo machete --with-metadata` - PASS/FAIL
- `./scripts/smoke-pty-runtime.sh` - PASS/FAIL
- `./scripts/compat-matrix.sh` - PASS/FAIL (if required)
- `bmux plugin rebuild --all-workspace-plugins` - PASS/FAIL (if required)

If any required command is skipped, explain why.
