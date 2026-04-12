# Mobile M0 Architecture (Android First, iOS Ready)

This document locks the M0 architecture for bmux mobile clients.

## Goals

- Ship a streamlined Android app focused on remote connection workflows.
- Keep iOS first-class by sharing core logic in Rust via FFI.
- Reuse existing bmux protocol/auth/target concepts instead of creating a parallel stack.
- Avoid design choices that block future terminal rendering on mobile.

## Non-Goals (M0)

- No terminal emulation UI in v1.
- No feature-parity replacement for full desktop CLI workflows.
- No Android-only protocol logic in shared Rust core.

## Platform Strategy

- Android UI: native Kotlin + Jetpack Compose.
- iOS UI (future): native SwiftUI.
- Shared logic: Rust crates exposed through UniFFI.

Why this split:

- Native UIs keep Android and iOS first-class and idiomatic.
- Rust core prevents duplicated protocol/network/security logic.
- Compose helps Android velocity and maintainability, independent of iOS.

## Monorepo Package Layout (M0)

Planned additions:

- `packages/mobile-core` (Rust): platform-neutral mobile domain logic.
- `packages/mobile-ffi` (Rust): UniFFI surface for Android/iOS bindings.
- `apps/android` (Kotlin/Compose): Android app and service lifecycle.

M0 intentionally defines contracts and boundaries first. Runtime implementation work starts in M1.

## Shared Rust Boundary

`mobile-core` owns:

- target parsing and normalization (`bmux://`, `iroh://`, `https://`, `tls://`, SSH target forms)
- connection orchestration and reconnect state machine
- transport adapters (iroh, TLS/LAN, SSH)
- session discovery/selection metadata calls
- diagnostics/error taxonomy for mobile UX

`mobile-core` does not own:

- Android/iOS UI or lifecycle primitives
- platform storage APIs directly
- terminal rendering widgets

## Existing bmux Reuse Points

The mobile core should be implemented by extracting/adapting existing logic from:

- `packages/cli/src/runtime/remote_cli.rs` target parsing and resolution
- `packages/cli/src/connection.rs` remote connection abstraction
- `packages/cli/src/ssh_access.rs` iroh auth/query semantics
- `packages/config/src/lib.rs` connection target model

Cluster gateway note:

- Current cluster gateway routing is plugin/CLI driven.
- Mobile v1 connection manager remains transport-centric; cluster behavior should remain transparent when connection targets resolve to normal URLs/targets.
- Preserve extension room for future cluster-aware UX by keeping target metadata extensible.

## Security and Trust Model

- TLS: strict verification by default.
- TLS pinning: supported as explicit per-target option.
- Local data: encrypted persistence for targets, recents, trust metadata, and connection prefs.
- Auth: reuse existing bmux auth and capability handshake model.
- Iroh auth: support `auth=ssh` semantics from iroh target URLs.

## Background Behavior (Android)

- Reconnect behavior can run with a foreground service.
- Shared Rust state machine remains lifecycle-agnostic.
- Android app layer maps service state to user-visible notifications and controls.

## Future Terminal Rendering Compatibility

To avoid blocking future terminal support:

- expose stream/event APIs that are UI-agnostic
- keep pane/session/event contracts transport-neutral
- avoid coupling connection APIs to current connection-only screens

This allows terminal UI to be added later without changing transport/auth architecture.

## Milestone Map

- M0: architecture and API contracts (this doc + FFI contract doc)
- M1: `mobile-core` crate scaffolding and target/connect state machine
- M2: SSH transport in shared Rust (no shelling out)
- M3: `mobile-ffi` UniFFI bindings
- M4: Android Compose app flows (targets, connect, session pick)
- M5: discovery, background reconnect, security hardening
- M6: alpha packaging and dogfooding
