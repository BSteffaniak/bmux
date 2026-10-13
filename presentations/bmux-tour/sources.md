# Claim and source checklist

Baseline: checkout `6d7c790b915f`, authored 2026-10-13. Links are repository-relative; recheck implementation and demonstrations before using a different build. A design invariant establishes the intended contract, not proof that every operational scenario has been tested.

| Slide / claim | Evidence | Presentation limit |
| --- | --- | --- |
| 1: Rust multiplexer + independent reusable UI | [README](../../README.md), [TUI guide](../../docs/tui-framework.md) | Early alpha; no invented original motivation or adoption statistics. |
| 2: session versus client view; personal versus shared arrangements | [Concepts](../../docs/concepts.md), [invariants](../../INVARIANTS.md), [CLI workflows](../../docs/cli-workflows.md) | Demo must show selection/focus independence, not falsely claim shared layout edits are private. Client disconnect is not machine restart. |
| 3: interpreted terminal state and attachment rendering | [Recording architecture](../../docs/recording-process.md), [attach runtime](../../packages/cli/src/runtime/attach/runtime.rs) | Diagram is conceptual; not every input invokes a domain service. |
| 4: typed plugin contracts and generic host boundary | [Plugin guide](../../docs/plugins.md), [architecture rules](../../AGENTS.md), [tabs schema](../../plugins/tabs-plugin-api/bpdl/tabs-plugin.bpdl) | Snippet is real `tabs-placement-v1`; capability policy and orchestration belong in plugin implementation. Do not claim all legacy migrations are finished. |
| 5: successful flush precedes presentation commit | [Recording contract](../../docs/recording-process.md), [attach runtime](../../packages/cli/src/runtime/attach/runtime.rs) | Failed presentation does not advance interaction authority; host output can still require repair. |
| 5: Kitty upload reuse versus destructive erase; raster cleanup | [Image performance](../../docs/image-performance.md), [images](../../docs/images.md), [attach regression tests](../../packages/cli/src/runtime/attach/runtime.rs) | Locate assertions `Kitty frame erased uploads` and `partial frame erased image`. No cross-terminal universal compatibility or speedup number is claimed. |
| 6: presentation reuses neutral TUI foundations | [Presenter library](../../packages/presenter/README.md), [presenter plugin](../../plugins/presenter-plugin/README.md), [TUI guide](../../docs/tui-framework.md) | Markdown is one adapter; Rust builders use the shared model. The CLI does not compile arbitrary Rust source. |
| 6: recording and optional browser rendering | [Recording](../../docs/recording-process.md), [browser rendering](../../docs/browser-rendering.md) | Text output is not a visual replay. Browser entry point has local/read-only and viewport-control constraints. |
| 7: local independence; federation fencing and recovery | [Invariants](../../INVARIANTS.md), [cluster implementation](../../plugins/cluster-plugin/src), [reconciler integration tests](../../plugins/cluster-plugin/src/worker_reconciler/integration_tests.rs) | Explain contracts and code, not unverified deployment maturity. Unreachability does not prove death; retries have a defined deduplication interval. |
| 8: limitations and engineering lessons | [README](../../README.md), [testing](../../TESTING.md), [image performance](../../docs/image-performance.md) | Closing roadmap is proposed presenter wording, not an externally promised release plan. |

## Feature map for questions

| Area | Where to explore |
| --- | --- |
| Sessions, tabs, workspaces, independent views | docs/concepts.md, docs/cli-workflows.md |
| Themes and configuration profiles | docs/config-profiles.md, docs/tui-framework.md |
| Plugins and revisioned presentation surfaces | docs/plugins.md, docs/tab-presentations.md |
| Images and host compatibility | docs/images.md, docs/image-performance.md |
| Recording and agent interaction | docs/recording-process.md, docs/playbooks.md |
| Browser view | docs/browser-rendering.md |
| Remote operations | docs/operations.md, docs/cli-workflows.md |
| Federation and execution fencing | INVARIANTS.md, plugins/cluster-plugin/src |

This is the breadth index, not a promise to cover every subsystem in 15 minutes.

## Approval / evidence gates

- [x] Source-grounded terminology and plugin ownership.
- [x] Explicit distinction between design contracts and operational claims.
- [x] No fabricated benchmark numbers, process restart survival, or demo captures.
- [ ] Presenter approves motivation and next-step wording.
- [ ] Exact demo build and input bindings rehearsed.
- [ ] Audience rendering and speaker-note privacy verified on real setup.
