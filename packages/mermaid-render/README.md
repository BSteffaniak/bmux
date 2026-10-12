# bmux_mermaid_render

Bounded Mermaid rendering extracted from Bcode's mermaid-render crate. Public
request/result/error/cache types own the contract; the concrete renderer backend
is private. No sessions, providers, UI effects, or image installation live here.

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

## Validation

All original verification/tampering, packaged snapshot lifecycle, cache, validation,
worker framing, malformed/oversized output, crash, timeout, cancellation, and memory
limit tests remain. The real worker integration tests use
`CARGO_BIN_EXE_bmux-mermaid-worker`. Workspace registration and the required Nix
clippy-before-nextest/machete checks are coordinator-owned; this extraction lane
ran no Cargo/build/test/fmt command.
