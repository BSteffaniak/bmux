# bmux_mermaid_render

Bounded Mermaid rendering extracted from Bcode's mermaid-render crate. Public
request/result/error/cache types own the contract; the concrete renderer backend
is private. No sessions, providers, UI effects, or image installation live here.

## Experimental embedded WASM

The opt-in `wasm` feature builds a locked, import-free guest and embeds portable
WASM plus destination-baseline AOT artifacts during ordinary Cargo builds. The
selected compiler needs `wasm32-unknown-unknown`; no separate guest build command
or runtime file is needed. `WasmMermaidRenderer::new()` loads trusted AOT;
`new_jit()` explicitly compiles portable WASM. Fuel variants are explicit.

Fresh bounded stores and epoch interruption settle execution before returning.
Async caller drop signals job-private cancellation without blocking the executor;
the admitted worker retains permits until it stops. See the guest README for
build isolation and portability patches.

This is not the production default: bundled font measurements differ from system
fonts used by the process backend. See
[performance and compatibility evidence](../../docs/mermaid-wasm-performance.md).
The process worker remains unchanged pending migration gates.

## API handoff

- `worker_main()` serves exactly one versioned request in a dedicated worker
  process. Both the standalone binary and downstream packaging shims call this
  public entry; its protocol implementation remains in a private module. It
  returns on success and attempts a framed failure response before exiting 1 on
  error. Do not invoke it in an interactive host or test harness process.
- `MermaidRenderRequest::svg`, `MermaidSource`, `MermaidRenderLimits`, and
  `MermaidCancellationToken` describe bounded cancellable work.
- `IsolatedMermaidRenderer::initialize_packaged(directory)` captures the selected
  installed worker. `packaged()` acquires a verified executable snapshot.
- `IsolatedMermaidRenderer::pin(path)` retains and verifies a private snapshot;
  `from_path(path)` and `render_mermaid_with_worker` support explicitly selected paths.
- Renderer output is `MermaidRenderedOutput::Svg`; diagnostics/failures are typed.
- `MermaidRenderCache` is reconstructible, bounded derived data (64 entries,
  32 MiB encoded output), never application authority.

Package the `bmux-mermaid-worker` binary beside the application or explicitly set
`BMUX_MERMAID_WORKER_PATH` before packaged initialization. Worker selection is
captured once; changing environment variables afterward does not redirect it.
Hosts own installation/bootstrapping and should use pinned/packaged execution
rather than mutable executable paths for verified operation.

`render_mermaid` is the synchronous backend entry used inside the worker. It is
not an isolation boundary; interactive applications must use the isolated adapter.
The adapter preserves concurrency permits, source/dimension/output limits,
request validation, directive rejection, timeout/cancellation, forceful child
termination, and Unix address-space / Windows job-object memory bounds. Unix
address-space enforcement is not a complete operating-system sandbox.

The existing version-1 `BCMW`/`BCMR` byte framing and cache-key semantics are
intentionally unchanged. Binary/crate/environment names are the only packaging
migration: later Bcode integration must select/package the new worker explicitly.

## Windows source handoff and external gate

Windows discovery selects `bmux-mermaid-worker.exe` beside the application (or the
captured explicit override) and snapshots it as `worker.exe`. The disposable copy
keeps its own writable attributes even when the installation is read-only, so
ordinary snapshot release can remove it. Digest checks still reject tampering.
Worker startup uses `CREATE_SUSPENDED`: Job Object memory and kill-on-close limits
are installed before the initial thread resumes. Thread discovery is bounded and
fails closed; any containment/resume failure terminates the suspended child. No
shell, PATH lookup, or breakaway flag is used. The protocol remains version 1.

The manifest-owned `supported_platforms` contract defaults to Unix-only;
presentation explicitly opts into Unix and Windows. `xtask` now builds opted-in
helpers, stages their `.exe` source/destination names, and verifies/copies them
with inventory identity. Unrelated Unix-only helpers remain unavailable on
Windows. The existing release and npm workflows already call `cargo xtask build`
and `cargo xtask auxiliary stage`; no workflow edits are needed for this wiring.
This is source wiring, **not evidence of a working Windows release bundle**.

External acceptance requires a native `x86_64-pc-windows-msvc` runner using the
repository-selected Rust version:

- Run `bmux_mermaid_render` unit/integration tests, including suspended containment,
  memory-limit rejection, descendant termination, read-only snapshot cleanup,
  native filename/override discovery, actual rendering, and tamper rejection.
- Run `bmux_plugin` auxiliary Windows tests (handle identity, denied replacement,
  junction/reparse rejection, installed inventory owner/digest/filename checks).
- Run `xtask` platform build/stage/install/verification tests.
- Build the complete release product, stage a Windows bundle, inspect
  `artifacts.json` for the declared `bmux-mermaid-worker.exe` SHA-256/size, install
  it, and exercise packaged rendering from that installed bundle, cancellation,
  timeout, and bounded malformed/oversized worker responses. Confirm the unrelated
  Unix-only supervisor remains listed unavailable.

The followup lane only inspected source and used focused rustfmt/diff checks on
Linux. It did not run Cargo builds/tests or produce/run a Windows bundle. Shared
clippy, nextest, machete and plugin rebuild validation remain coordinator-owned.

## Validation

All original verification/tampering, packaged snapshot lifecycle, cache, validation,
worker framing, malformed/oversized output, crash, timeout, cancellation, and memory
limit tests remain. The real worker integration tests use
`CARGO_BIN_EXE_bmux-mermaid-worker`. Workspace registration and the required Nix
clippy-before-nextest/machete checks are coordinator-owned; this extraction lane
ran no Cargo/build/test/fmt command.
