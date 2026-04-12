# ADR-0001: Native Mobile UIs with Shared Rust Core

- Status: accepted
- Date: 2026-04-12

## Context

bmux needs a mobile client that is fast to ship on Android while remaining iOS-ready.
The product goal is connection-first remote workflows, with optional future terminal rendering.

## Decision

Use native platform UIs with a shared Rust core over FFI:

- Android UI: Kotlin + Jetpack Compose
- iOS UI: SwiftUI (future)
- Shared domain logic: Rust crates exposed via UniFFI

## Rationale

- Keeps Android and iOS as first-class citizens.
- Avoids duplicating protocol/auth/transport behavior across platform codebases.
- Uses Compose where it is strongest (Android UX and development speed).
- Preserves flexibility for future terminal UI without reworking transport core.

## Consequences

Positive:

- Single source of truth for target parsing, connection state, auth, and security decisions.
- Better cross-platform correctness and easier protocol evolution.
- Lower long-term maintenance cost than duplicated native network stacks.

Tradeoffs:

- Requires careful FFI API design and versioning discipline.
- Build/test pipelines become multi-language.
- Mobile-specific lifecycle concerns still live in each platform app.

## Alternatives Considered

- Compose Multiplatform UI for both platforms: rejected for now due to iOS maturity/risk profile for this project phase.
- Fully separate native implementations: rejected due to logic drift and duplicated maintenance.

## Follow-Up

- M1: scaffold `packages/mobile-core` and `packages/mobile-ffi`.
- M1: define initial stable FFI types and error codes.
- M2+: implement SSH transport in Rust (no subprocess `ssh` dependency on mobile).
