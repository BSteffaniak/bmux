# AGENTS.md

This file defines REQUIRED validation steps for coding agents working in this repo.

## Core Rule

If you change code, run the relevant tests before finishing and report exactly what you ran and whether it passed.

## Minimum Required Commands (CLI/runtime work)

For changes in `packages/cli/**` (runtime, input, pane/layout, protocol, terminal handling), run:

1. `cargo test -p bmux_cli`
2. `cargo check -p bmux_cli`
3. `./scripts/smoke-pty-runtime.sh`

## Compatibility-Related Changes (REQUIRED extra check)

If the change touches terminal protocol behavior, TERM/profile logic, or query/reply handling, also run:

4. `./scripts/compat-matrix.sh`

This includes edits under:

- `packages/cli/src/runtime/terminal_protocol.rs`
- `packages/cli/src/runtime/pane_runtime.rs`
- `packages/cli/src/runtime/mod.rs` (terminal doctor / profile / term resolution)
- `packages/config/src/lib.rs` (`behavior.pane_term`, trace flags, profile settings)

## Change-to-Tests Mapping

- Protocol/query/reply/TERM/profile changes
  - Run all 4 commands above.
- Input/keymap/runtime command handling changes
  - Run 1-3.
- Layout/pane lifecycle/compositor changes
  - Run 1-3.
- Config-only changes
  - Run at least 1-2; include 3 if behavior affects runtime startup.
- Docs-only changes
  - No mandatory runtime commands.

## Completion Reporting Format

Agents should report test execution in final response using this format:

- `cargo test -p bmux_cli` - PASS/FAIL
- `cargo check -p bmux_cli` - PASS/FAIL
- `./scripts/smoke-pty-runtime.sh` - PASS/FAIL
- `./scripts/compat-matrix.sh` - PASS/FAIL (if required)

If any required command is skipped, explain why.
