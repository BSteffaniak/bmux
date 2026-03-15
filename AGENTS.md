# AGENTS.md

This file defines REQUIRED validation steps for coding agents working in this repo.

## Core Architecture Boundary (REQUIRED)

BMUX core must remain domain-agnostic. Windows and permissions are plugin domains, not core architecture.

### Hard Rules

- Do not add or keep windows-domain or permissions-domain logic in core architecture layers.
- Core architecture includes at least:
  - `packages/server/**`
  - `packages/client/**`
  - `packages/ipc/**`
  - `packages/session/**`
  - `packages/terminal/**`
  - `packages/event/**`
- In core architecture, avoid domain-specific types/fields/events/APIs for windows or roles/permissions.
- Domain behavior must be implemented through plugins and generic plugin/service invoke paths.
- Core defaults when plugins are missing:
  - Missing windows plugin: baseline single terminal attach/session/pane flow still works.
  - Missing permissions plugin: permissive single-user behavior.

### Plugin Power Expectations

- Plugins are first-class and may implement critical product behavior.
- Prefer extending generic plugin APIs/capabilities over adding core-special-case code.
- If a feature seems domain-specific, place it in a plugin unless there is a strong, documented reason it must be core-agnostic runtime plumbing.

### Review Gate Before Finishing (REQUIRED)

For any non-doc code change, verify no forbidden domain leakage was introduced in core architecture:

- Run content checks (or equivalent) to confirm no new core references to windows/permissions domain concepts.
- If any are found, treat as blocking and refactor before finishing.

These boundary rules are strict and take precedence over convenience.

## Core Rule

If you change code, run the relevant tests before finishing and report exactly what you ran and whether it passed.

In addition, for any code change anywhere in the repo, run:

- `cargo nextest run --no-fail-fast`

This is required. Treat failures as blocking, and do not finish with known flaky/failing tests.

## Plugin Changes (REQUIRED)

If a change touches `plugins/**`, rebuild bundled plugins before finishing:

- `./scripts/rebuild-bundled-plugins.sh`

During iteration you may run targeted rebuilds with selectors (plugin id, short name, or crate name), but final validation for plugin changes should run the command without selectors.

## Minimum Required Commands (CLI/runtime work)

For changes in `packages/cli/**` (runtime, input, pane/layout, protocol, terminal handling), run:

1. `cargo nextest run --no-fail-fast`
2. `cargo test -p bmux_cli`
3. `cargo check -p bmux_cli`
4. `./scripts/smoke-pty-runtime.sh`

## Compatibility-Related Changes (REQUIRED extra check)

If the change touches terminal protocol behavior, TERM/profile logic, or query/reply handling, also run:

5. `./scripts/compat-matrix.sh`

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
  - Run `./scripts/rebuild-bundled-plugins.sh`.
- Docs-only changes
  - No mandatory runtime commands.

## Completion Reporting Format

Agents should report test execution in final response using this format:

- `cargo test -p bmux_cli` - PASS/FAIL
- `cargo check -p bmux_cli` - PASS/FAIL
- `cargo nextest run --no-fail-fast` - PASS/FAIL
- `./scripts/smoke-pty-runtime.sh` - PASS/FAIL
- `./scripts/compat-matrix.sh` - PASS/FAIL (if required)
- `./scripts/rebuild-bundled-plugins.sh` - PASS/FAIL (if required)

If any required command is skipped, explain why.
