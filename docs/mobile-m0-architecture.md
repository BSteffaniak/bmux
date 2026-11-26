# Mobile M0 Architecture (Android First, iOS Ready)

This document locks the M0 architecture for bmux mobile clients.

## Goals

- Ship a streamlined Android app focused on remote connection workflows.
- Keep iOS first-class by sharing core logic in Rust via FFI.
- Reuse existing bmux protocol, pairing and connector code instead of creating a parallel stack.
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

M0 intentionally defines contracts and boundaries first. Runtime implementation
work starts in M1.

## Shared Rust Boundary

`mobile-core` owns:

- the paired-host model: pairing state, saved hosts, connect (over the
  connections plugin's `paired_hosts` module)
- connection orchestration and reconnect state machine
- session discovery/selection metadata calls
- diagnostics/error taxonomy for mobile UX

`mobile-core` does not own:

- Android/iOS UI or lifecycle primitives
- platform storage APIs directly
- terminal rendering widgets

## Existing bmux Reuse Points

The mobile core reuses desktop client code directly rather than extracting it:

- `bmux_connections_plugin::paired_hosts`: pairing link checks, the
  `bmux/pair/1` pairing client, the known-hosts store, the device key and the
  saved-host connector (the same code `bmux connect` uses)
- `bmux_client`: the attach protocol over the connector's stream

Cluster note: a phone pairs with each host it reaches; cluster membership is
never granted by pairing.

## Security and Trust Model

- Pairing: a single-use 256-bit link secret proven over a transcript bound to
  both keys, plus a phrase both devices show; the host owner approves.
- Host identity: the host key is pinned at pairing; a changed key is a hard
  error, never a silent re-pair.
- Device identity: one device key per app install, kept out of backups.
- Authorization: the host enforces the device's role (`full` or `view-only`).

## Background Behavior (Android)

- Reconnect behavior can run with a foreground service.
- Shared Rust state machine remains lifecycle-agnostic.
- Android app layer maps service state to user-visible notifications and controls.

## Future Terminal Rendering Compatibility

To avoid blocking future terminal support:

- expose stream/event APIs that are UI-agnostic
- keep pane/session/event contracts transport-neutral
- avoid coupling connection APIs to current connection-only screens

This allows terminal UI to be added later without changing transport/auth
architecture.

## Milestone Map

- M0: architecture and API contracts (this doc + FFI contract doc)
- M1: `mobile-core` crate scaffolding and target/connect state machine
- M2: SSH transport in shared Rust (since removed)
- M3: `mobile-ffi` UniFFI bindings
- M4: Android Compose app flows (targets, connect, session pick)
- M5: discovery, background reconnect, security hardening
- M6: alpha packaging and dogfooding
- Paired hosts: SSH/TLS/raw-Iroh targets replaced by pairing and saved hosts
