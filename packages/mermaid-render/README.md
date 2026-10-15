# Embedded Mermaid rendering

`bmux_mermaid_render` renders exclusively inside an import-free WASM guest.
There is no native renderer, subprocess protocol, worker executable, executable
snapshot, or worker-path environment override.

## Build and deployment

Ordinary Cargo builds compile the independently locked guest and embed portable
WASM plus destination-baseline AOT artifacts. The selected compiler must include
`wasm32-unknown-unknown`. Cranelift destination backends are enabled for release
cross-compilation. No runtime guest file, Rust toolchain, network connection or
helper executable is required on deployed machines.

`WasmMermaidRenderer::new()` loads trusted embedded AOT; `new_jit()` compiles the
same embedded portable WASM. Both execute the same guest and enforce the same
contract. Fuel budgets are explicitly opt-in.

## Browser boundary

Browser (`wasm32`) consumers share request/cache models but do not link Wasmtime
or compile a nested guest. Rendering occurs on the owner host; browser viewers
consume prepared SVG. Calling `render_mermaid` in a browser build returns an
explicit renderer-unavailable error after validation. The web asset build guards
against pulling the native Wasmtime runtime into the browser dependency graph.

## API and bounds

- `render_mermaid` is the synchronous convenience entry backed by a shared WASM
  engine. Markdown's inline rendering uses this entry.
- `WasmMermaidRenderer::{render, render_async}` reuse compiled code while each
  request gets a fresh store. Presenter supplies this renderer directly.
- `MermaidRenderRequest`, `MermaidRenderLimits`, `MermaidCancellationToken`, typed
  results and optional bounded cache are BMUX-owned contracts.
- Source is capped at 64 KiB, output at 4 MiB, guest memory at 512 MiB, and async
  admission at eight jobs. Requested limits can be stricter.
- Epoch interruption checks per-job cancellation and deadlines. Execution settles
  before synchronous return; dropping an async waiter cancels its own job without
  blocking the executor. Permits remain held until actual execution stops.
- Directives are rejected. Errors never trigger a native/process fallback.

The guest uses fixed bundled font metrics. Presenter rasterization selects bundled
fonts and installed system fallback; glyph coverage and complex-script layout are
not universal. Minor native layout differences are not compatibility failures.
See `guest/README.md` for fork and font provenance.

## Verification and measurements

Run `cargo nextest run -p bmux_mermaid_render --no-fail-fast` for ABI, limits,
interruption, destination AOT and worker-free relocated executable tests.
`examples/compare.rs` and `scripts/benchmark-mermaid.py` measure only WASM AOT/JIT
modes. Historical process comparisons in `docs/mermaid-wasm-performance.md` are
retained as historical evidence, not current invocation instructions.
