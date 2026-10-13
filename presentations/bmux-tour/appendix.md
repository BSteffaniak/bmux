---
id: bmux-tour-appendix
title: BMUX — technical appendix
author: Braden
theme: dark
---
<!-- bmux:slide-id vocabulary -->
# Vocabulary and ownership

- Runtime: isolated execution environment.
- Session: execution lifetime; pane: terminal surface.
- Tab: plugin-owned attach unit; workspace: grouping of tabs.
- Client/attachment: viewer, association, and local interaction.

Personal arrangements differ from deliberately shared arrangements.

<!-- bmux:notes -->
Use docs/concepts.md as vocabulary authority. Independence is not a promise that deliberate shared arrangement mutations are invisible to other clients.
<!-- bmux:end-notes -->

---
<!-- bmux:slide-id plugin-contracts -->
# Contract versus implementation

BPDL schema → generated typed services / clients / events

Stable API crate ≠ runtime implementation crate.

Host primitives: storage, logging, recording, generic dispatch.

Domain policy and orchestration stay with the owning plugin.

<!-- bmux:notes -->
Sources: AGENTS.md, docs/plugins.md, plugins/tabs-plugin-api/bpdl/tabs-plugin.bpdl. Foundational plugins can reach generic kernel requests; that does not make a permission/tab convenience API appropriate in core. Describe the boundary, not a claim that every migration is complete.
<!-- bmux:end-notes -->

---
<!-- bmux:slide-id image-lifecycle -->
# Image protocols have different lifecycles

| Host output | Reuse | Cleanup |
| --- | --- | --- |
| Kitty | upload once, place again | placements versus content |
| Sixel | cached encoding, emit again | raster repair + repaint |
| iTerm2 | cached encoding, emit again | raster repair + repaint |

Write/flush failure leaves host state uncertain—not committed.

<!-- bmux:notes -->
These are BMUX strategy summaries, not full protocol specifications. Host behavior varies. Recent fixes distinguish retained placement resources from raster footprints; visual testing remains important. Source paths are listed in sources.md.
<!-- bmux:end-notes -->

---
<!-- bmux:slide-id reusable-ui -->
# Reusable UI stack

Primitives: geometry, buffers, painting, input, damage.

Components: scrolling, text, selection, composite controls.

Runtime: scheduling, input admission, presentation lifecycle.

Applications: state, commands, and product-specific behavior.

<!-- bmux:notes -->
Sources: docs/tui-framework.md and packages/tui, tui-components, tui-runtime. Dependency direction matters: primitives do not depend on product components or runtime.
<!-- bmux:end-notes -->

---
<!-- bmux:slide-id distributed-details -->
# Recovery is an ownership problem

Durable intent → deterministic consensus apply → reconciliation.

- Same retry identity: same logical outcome within the defined window.
- Conflicting identity reuse: reject.
- New execution generation: fence old workers.
- Unreachable worker: investigate; do not infer death.

<!-- bmux:notes -->
Sources: INVARIANTS.md, plugins/cluster-plugin/src, cluster integration tests. This is not a claim that every external effect can be rolled back or that arbitrary processes survive host restart.
<!-- bmux:end-notes -->

---
<!-- bmux:slide-id evidence -->
# How I check the system

- Deterministic state, protocol, and pixel assertions.
- PTY integration and terminal compatibility scenarios.
- Fault injection: output failure, retries, recovery.
- Opt-in benchmarks: work counts + reproducible measurements.
- Real terminal rehearsal: the user-visible acceptance check.

Passing unit tests do not prove every terminal behaves correctly.

<!-- bmux:notes -->
Sources: AGENTS.md, TESTING.md, docs/image-performance.md. Do not put a fixed test count on this slide: the suite changes. Distinguish performance measurements from correctness acceptance and platform-specific coverage.
<!-- bmux:end-notes -->
