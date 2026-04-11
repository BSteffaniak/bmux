# BMUX Plugin Architecture Decisions

This document captures the agreed architecture direction for BMUX plugins so future work stays consistent.

## Purpose

- Keep BMUX core domain-agnostic.
- Let plugins extend core mechanics through generic APIs.
- Enforce access with policy/capabilities, not hardcoded domain logic.

## Core Principles

- Core must not contain windows-domain or permissions-domain behavior.
- Plugins are first-class and can implement critical product behavior.
- Any domain feature should be built in plugins unless there is a strong reason for core runtime plumbing.
- Runtime core scope includes `packages/cli/src/runtime/**` (Option B boundary).

## Context Model (Canonical)

`Context` is the generic, attachable execution resource in core.

### What a Context Represents

A context is not a session and not a window by definition. It is a composable workspace primitive that can back many plugin concepts (windows, tabs, views, workspaces, etc.).

Each context owns at least:

- pane tree/layout
- focused pane
- attach routing target
- per-context runtime/view state

### Identity and Sharing

- `ContextId` is globally unique (UUID).
- Contexts are shareable across plugins.
- Core does not hardcode one plugin as owner of contexts.

### Attributes

Contexts include `attributes: map<string,string>` for plugin coordination and metadata.

Attributes are for discovery/coordination hints, not direct security policy decisions.

Recommended naming:

- `core.*` reserved for core-defined keys
- `<plugin_id>.*` for plugin-defined keys

## Session Relationship

- Contexts are not always scoped to sessions.
- Core should support contexts as first-class resources without mandatory session ownership.
- Session behavior may itself become plugin-owned in the future.

## Activation and Close Semantics

- On close of the active context, select the most-recent-active context (MRU).
- `ContextClose` supports `force`.

## Plugin API Direction

Expose generic host service interfaces for context operations:

- `context-query/v1`
- `context-command/v1`

Use typed `bmux_plugin_sdk` host runtime APIs for all plugin access to core mechanics.

## Command Outcome Contract

Plugin command execution should support a generic outcome contract (for keybinding/runtime flows), including selecting a target context after command success.

This enables behavior like `ctrl-a c` to create and immediately switch to a newly created context without embedding windows-domain logic in core runtime.

## Mouse Gesture Mapping (Config Snippet)

Mouse gestures can trigger built-in runtime actions or plugin commands through
`behavior.mouse.gesture_actions`.

```toml
[behavior.mouse]
enabled = true
focus_on_click = true
click_propagation = "focus_and_forward"
focus_on_hover = false
scroll_scrollback = true
wheel_propagation = "forward_only"
scroll_lines_per_tick = 3
exit_scrollback_on_bottom = true

[behavior.mouse.gesture_actions]
click_left = "plugin:bmux.windows:new-window"
hover_focus = "focus_next_pane"
scroll_up = "scroll_up_line"
scroll_down = "scroll_down_line"
```

Supported gesture keys in current core runtime:

- `click_left`
- `hover_focus`
- `scroll_up`
- `scroll_down`

## Permissions and Policy

- Enforcement is config/policy-file driven and non-interactive for now.
- No interactive permission prompts at this stage (may be added later).
- Policy actions should be explicit, no aliases.

Examples of explicit action style:

- `context.create`
- `context.select`
- `context.close`
- `context.list`

## Windows Plugin Mapping

Windows is a plugin UX/domain concept. It should map to generic contexts rather than forcing core windows types.

Expected behavior:

- `new-window` creates a context
- `switch/next/prev/last-window` select contexts
- `kill-window` closes a context
- `ctrl-a c` immediately switches attach context to the newly created context

## Guardrails and Validation

- Keep architecture guardrail tests blocking for domain leakage in core and runtime production paths.
- Keep parity contract tests for bundled windows/permissions command surfaces.
- Required validation for runtime/code changes follows `AGENTS.md`.

## Routing Policy (Config)

Command ownership requirements are host-policy driven, not hardcoded by plugin ID.

```toml
[plugins.routing]
conflict_mode = "fail_startup"

[[plugins.routing.required_namespaces]]
namespace = "plugin"

[[plugins.routing.required_paths]]
path = ["terminal", "doctor"]
```

Claims may optionally pin ownership to a specific plugin:

```toml
[[plugins.routing.required_namespaces]]
namespace = "playbook"
owner = "example.playbook"
```

Resolution behavior is deterministic:

- exact path claim takes precedence over namespace claim
- conflicting plugin claims fail startup
- unmet required claims fail startup

## Compatibility Policy

- Pre-baseline plugin command bridge behavior is intentionally unsupported (clean break).
- Current baseline is versioned and explicit:
  - capability: `bmux.commands`
  - service interface: `cli-command/v1`
  - operation: `run_path`
  - bridge protocol marker: `BMUXCMD1`
  - bridge protocol version: `1`
- Future compatibility changes should be additive:
  - add `.../v2` interfaces or operations, do not mutate `v1` semantics silently
  - negotiate by advertised capabilities/interfaces before selecting newer versions
  - keep compatibility seams in shared constants/helpers rather than ad-hoc call sites

## Process Runtime Protocol v1

`runtime = "process"` plugins communicate with BMUX over framed stdio messages.

- transport marker: `BMUXPRC1`
- frame layout: `<magic><u32_be_payload_len><payload_bytes>`
- payload encoding: service codec message (`encode_service_message` / `decode_service_message`)
- protocol version field in request/response envelopes: `1`

Environment passed to the process runtime:

- `BMUX_PLUGIN_RUNTIME_PROTOCOL=stdio-v1`
- `BMUX_PLUGIN_ID=<plugin-id>`
- `BMUX_PLUGIN_RUNTIME_PERSISTENT_WORKER=1` (only when `process_persistent_worker = true`)

Process runtime manifest knobs:

- `entry` - process command/path to execute
- `entry_args` - default process arguments
- `process_persistent_worker = true|false` - optional worker mode (reuse one process for multiple invocations)

Runtime behavior and constraints:

- `stdout` is reserved for framed protocol responses only.
- non-protocol diagnostics should be written to `stderr`.
- host enforces a process timeout (default 30000ms).
- timeout may be overridden with `BMUX_PROCESS_PLUGIN_TIMEOUT_MS`.
- if a process exits without framed `stdout`, host treats it as unsupported for framed operations.

Examples:

- `examples/process-plugin-node/`
- `examples/process-plugin-python/`

These examples focus on frame transport and process lifecycle behavior and emit
BMUX service-codec-compatible response payloads.

Troubleshooting:

- error: `missing BMUXPRC1 frame prefix`
  - cause: process emitted non-protocol bytes to stdout
  - fix: write diagnostics to stderr only; keep stdout framed responses only
- error: `truncated frame header` or `truncated payload`
  - cause: incomplete write to stdout
  - fix: write a single complete frame and flush stdout before exit
- error: process entry is not executable
  - cause: entry path exists but lacks execute permissions
  - fix: `chmod +x <entry>` (or use a launch command like `python3` with script args)
- error: process plugin timed out
  - cause: process did not return in time
  - fix: optimize startup/handler path or increase `BMUX_PROCESS_PLUGIN_TIMEOUT_MS`

Versioning policy for process runtime mirrors other plugin compatibility rules:

- keep `v1` semantics stable once published
- introduce `v2+` as additive protocol envelopes/operations
- gate newer behavior via explicit protocol version/capability checks

## Migration Direction

As context substrate work lands:

- move pane/layout ownership to context runtime structures
- add context IPC/client/plugin host primitives
- keep fallback behavior when plugins are missing
- add persistence migration from legacy single-target state to default context state

## Status

This document reflects current agreed decisions from architecture discussions and should be updated whenever these decisions change.
