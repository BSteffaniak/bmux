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

## Migration Direction

As context substrate work lands:

- move pane/layout ownership to context runtime structures
- add context IPC/client/plugin host primitives
- keep fallback behavior when plugins are missing
- add persistence migration from legacy single-target state to default context state

## Status

This document reflects current agreed decisions from architecture discussions and should be updated whenever these decisions change.
