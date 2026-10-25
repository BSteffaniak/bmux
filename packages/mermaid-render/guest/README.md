# Portable Mermaid guest

The guest workspace is materialized under `OUT_DIR/guest` from `Cargo.toml.in`,
`Cargo.lock.in`, and `src/lib.rs`. Templates prevent Cargo from excluding it as
a
nested package from the published archive. Ordinary native host builds compile
its locked release artifact for `wasm32-unknown-unknown` and embed
`OUT_DIR/mermaid.wasm`.
The build also precompiles epoch-only and fuel-enabled AOT artifacts for Cargo's
explicit destination `TARGET`, using `wasm_config.rs` shared with the runtime.
Browser (`wasm32`) host builds skip the nested guest. No target, compiler, font,
or renderer is installed by the build script. Cargo retains its normal
network/offline/cache behavior; offline builds require locked dependencies
cached.

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
manifest, lockfile, guest source, font, and license additions/removals.

## ABI v1

Exported memory plus `alloc(u32)->u32`, `dealloc(u32,u32)`, and
`render(u32,u32)->u64`. The render result packs pointer HIGH 32, length LOW 32.
Input is UTF-8 Mermaid. Output is one status byte (0 SVG, 1 error), then UTF-8.
The host frees input independently and frees output with its exact returned
length. Panics/resource exhaustion trap and must be handled by the host.

## Renderer provenance and portability fixes

The guest consumes <https://github.com/BSteffaniak/mermaid-rs-renderer>, branch
`wasm-portability`. Cargo.lock retains the exact resolved commit (currently
`7c06c9ab2c69da15f356ccfc5c27d1b77f59bec4`). Advance it explicitly with
`cargo update --manifest-path "$OUT_DIR/guest/Cargo.toml" -p
mermaid-rs-renderer`
after a host build, then copy the updated lockfile back to `Cargo.lock.in`;
ordinary locked builds do not automatically follow branch updates.
It is based on upstream release 0.3.1 commit
`2f993bd79a55235eb59a34d807852276ba25bea7`; no unrelated master updates are
included.
There is no vendored source or submodule. The host workspace contains no native
Mermaid renderer; this guest is the
sole renderer implementation.

Fork changes are general-purpose: clock-free diagnostic timings on wasm32
unknown,
no system-font/cache IO on that target, and `set_text_measurement_font`
accepting
caller-supplied font bytes. Actual import-free execution tests live in the fork.
No BMUX font or runtime policy lives there. The guest initializes its locally
bundled font once per fresh instance through the public API.

Offline builds require both Git and registry dependencies to be cached. To
return
to upstream after a compatible release, replace the guest workspace dependency
with that crates.io version and refresh its lockfile; the guest font API call
and
runtime policy need not change. No upstream PR has been created.

## Font provenance and fidelity

The guest and presenter use identical `bmux_fonts::DIAGRAM_FONT`
bytes. The host build stages those bytes with the guest sources; no sibling
checkout path or host-crate dependency is needed inside the guest. The guest
explicitly sets its SVG theme family to `BMUX Diagram`; the
presenter selects that regular face for measurement-compatible label painting.
The derived font includes technical-label alphabets plus monochrome symbols and
emoji. See `packages/fonts/assets/diagram/README.md` for pinned inputs, hashes,
regeneration and SIL OFL licenses. No fonts are fetched at runtime.

System fallback may extend script coverage but does not guarantee matching
metrics for unsupported scripts. Full CJK/RTL shaping and complex emoji ZWJ
sequences remain outside the bundled-font guarantee. Browser builds receive
owner-prepared SVG; they do not execute a nested Wasmtime runtime.

## Validation

```sh
CARGO_TARGET_DIR="$OUT_DIR/guest-target" cargo clippy --manifest-path "$OUT_DIR/guest/Cargo.toml" --target wasm32-unknown-unknown --release --locked --offline --all-targets -- -D warnings
CARGO_TARGET_DIR="$OUT_DIR/guest-target" cargo build --manifest-path "$OUT_DIR/guest/Cargo.toml" --target wasm32-unknown-unknown --release --locked --offline
node packages/mermaid-render/guest/tests/abi.cjs "$OUT_DIR/guest-target/wasm32-unknown-unknown/release/bmux_mermaid_guest.wasm"
```

The Node probe checks actual rendering, no imports, packed ABI, independent
buffer ownership, UTF-8 errors, reuse, and deterministic repeated instances.
Host Wasmtime integration is covered by `../tests/build_pipeline.rs`; running
outer-workspace validation remains the coordinator's responsibility.
