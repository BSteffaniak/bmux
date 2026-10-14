# Portable Mermaid guest

This standalone workspace is a concrete build input of `../build.rs`, not a
runtime package. Ordinary host builds with feature `wasm` compile its locked
release artifact for `wasm32-unknown-unknown` and embed `OUT_DIR/mermaid.wasm`.
The build also precompiles epoch-only and fuel-enabled AOT artifacts for Cargo's
explicit destination `TARGET`, using `wasm_config.rs` shared with the runtime.
Cross-architecture AOT is currently rejected explicitly: this prototype enables
only Wasmtime's host compiler backend. Enabling Wasmtime 38.0.4 `all-arch` hit an
unpublished `winch-codegen =38.0.4` dependency during resolution; it is not enabled.
Build and runtime still share explicit destination-baseline settings. Platform
execution and cross-architecture packaging require separate validation.
Without that feature the build script does not invoke guest Cargo. No target,
compiler, font, or renderer is installed/downloaded by the build script. Cargo
retains its normal offline/cache/registry behavior; offline builds require the
locked registry dependencies already cached.

## Isolation exception

A standalone workspace and its own workspace dependency table avoid recursively
building or locking the host workspace. The deliberately isolated nested Cargo
target directory lives under the outer `OUT_DIR`, not the shared host target
root. The outer selected `RUSTC` is preserved, including the Nix-selected
compiler. Host codegen flags/wrappers/profile overrides and jobserver variables
are removed; the guest gets explicit target/release/locked/jobs=1 settings.
No host crate is a guest dependency. Concurrent outer builds use their own
Cargo-managed `OUT_DIR`; identical package builds are serialized by Cargo.
The build script recursively tracks the entire guest tree, including sources,
manifest, lockfile, vendored source, font, and license additions/removals.

## ABI v1

Exported memory plus `alloc(u32)->u32`, `dealloc(u32,u32)`, and
`render(u32,u32)->u64`. The render result packs pointer HIGH 32, length LOW 32.
Input is UTF-8 Mermaid. Output is one status byte (0 SVG, 1 error), then UTF-8.
The host frees input independently and frees output with its exact returned
length. Panics/resource exhaustion trap and must be handled by the host.

## Renderer provenance and patch scope

`vendor/mermaid-rs-renderer` contains the complete `src` tree from crates.io
`mermaid-rs-renderer` 0.3.1, MIT license, README, and original manifest.
Upstream: <https://github.com/1jehuang/mermaid-rs-renderer>.
Original `.crate` SHA-256:
`6cb5b469222ce6b9896edf0a969cf68464f5a028ef394fd6643f00bd943df117`.
The build manifest retains upstream library/feature/dependency definitions;
unused dev-only benchmarks/profile are omitted. Upstream test/example/benchmark
assets are not vendored; this is a complete library implementation, not a copy
of the upstream test suite. The vendor is excluded from first-party workspace
lint/test targets, as a third-party dependency, not given blanket lint allows.
Version remains exactly 0.3.1, selected by guest workspace dependency and lock.
No global registry or native renderer is modified.

Source changes are limited to:

- New private `diagnostic_clock.rs`, imported by `lib.rs`, `layout/mod.rs`, and
  `layout/flowchart/edge_pipeline.rs`. On wasm32 unknown only, `now` and `elapsed`
  report zero duration instead of trapping. Other targets retain std Instant.
  Audited timing uses only accumulate/report parse/layout/render and stage
  durations; no layout/search/routing decisions consume these durations.
  Optional CLI code is disabled and unchanged.
- `text_metrics.rs`: wasm32 unknown loads the embedded face through the existing
  ttf-parser measurement path, bypassing private system-font discovery and disk
  cache. Native branches remain unchanged. Missing/invalid bundled font traps
  rather than silently falling back. Upstream missing-glyph/CJK heuristics remain.

## Font provenance and fidelity

`fonts/JetBrainsMonoNerdFont-Regular.ttf` is copied byte-for-byte from
`packages/fonts/assets/nerd-fonts/JetBrainsMonoNerdFont-Regular.ttf`, with its
existing `packages/fonts/licenses/JETBRAINS_MONO_OFL.txt` license copied beside
it. Font SHA-256:
`0ec29a68b539ece7078fc714cebff0c0accb2f4948f8f7963d9f5e86633b12d9`.

Measurements use this fixed regular font regardless of requested family.
This deliberately differs from the current native system-font baseline, so
SVG geometry need not match native. SVG font-family styling is not rewritten;
a later SVG rasterizer/viewer may substitute fonts. Deterministic measurement
is not a claim of pixel-identical rendering or complete Unicode glyph coverage.
The font bytes are compiled into WASM; there are no WASI/filesystem/network
imports or runtime font files. AOT remains deferred.

## Focused validation

In the Nix/direnv shell, using an isolated build-artifact directory:

```sh
CARGO_TARGET_DIR="$OUT_DIR/guest-target" cargo clippy --manifest-path packages/mermaid-render/guest/Cargo.toml --target wasm32-unknown-unknown --release --locked --offline --all-targets -- -D warnings
CARGO_TARGET_DIR="$OUT_DIR/guest-target" cargo build --manifest-path packages/mermaid-render/guest/Cargo.toml --target wasm32-unknown-unknown --release --locked --offline
node packages/mermaid-render/guest/tests/abi.cjs "$OUT_DIR/guest-target/wasm32-unknown-unknown/release/bmux_mermaid_guest.wasm"
```

The Node probe checks actual rendering, no imports, packed ABI, independent
buffer ownership, UTF-8 errors, reuse, and deterministic repeated instances.
Host Wasmtime integration is covered by `../tests/build_pipeline.rs`; running
outer-workspace validation remains the coordinator's responsibility.
