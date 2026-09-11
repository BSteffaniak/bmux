# Nix-owned development toolchain

`flake.lock` is the Rust toolchain authority. The flake selects stable Rust from
its locked rust-overlay revision, not the current rustup stable channel. Update
that input deliberately with `nix flake update rust-overlay`, then validate the
new compiler. There is no independently maintained rust-toolchain.toml.

## Terminals, scripts, and agents

Use direnv (`direnv allow`) or enter `nix develop`. Noninteractive entry points:

```sh
nix develop --command cargo clippy --all-targets -- -D warnings
bash scripts/dev.sh cargo nextest run --no-fail-fast
bash scripts/dev.sh ./scripts/smoke-pty-runtime.sh
```

Launch editors from the shell (`nix develop --command code .`, for example) so
rust-analyzer and its Cargo subprocesses inherit the compiler selection. Restart
existing editor processes after a toolchain update; opening a new terminal does
not update an already-running language server. The shell includes rust-analyzer,
clippy, rustfmt, cargo-nextest, and cargo-machete.

The shell explicitly selects rustc/rustdoc, verifies Rust executable provenance,
and rejects inherited compiler wrappers. Build artifacts live in
`target/nix/<toolchain-store-identity>` for this checkout. Different Nix compilers
and ordinary non-Nix builds do not share that directory. Scripts must honor
`CARGO_TARGET_DIR`; use `$CARGO_TARGET_DIR/debug/bmux` inside the shell rather
than the legacy `target/debug/bmux` path. Old artifacts need not be deleted.

The shell does not intercept arbitrary commands launched outside it. Such builds
are not the supported development entry point. A conflicting compiler wrapper
must be explicitly unset before entering the shell.

## CI and native platforms

The reusable `rust-version.yml` workflow evaluates the exact Rust version from
this checkout's locked flake. Existing native CI and release jobs install that
version through rustup, including Windows, where Nix cannot provide a native
shell. This is a transport of the Nix decision, not an independently floating
version. Cross-compilation target installation remains job-specific.

A source pin is not updated automatically by development environment entry.
No runtime, user configuration, or build cache is deleted by this setup.
