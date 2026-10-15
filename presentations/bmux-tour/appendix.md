---
id: bmux-tour-appendix
title: BMUX — implementation follow-ups
author: Braden
theme: dark
---
<!-- bmux:slide-id vocabulary -->
<!-- bmux:block-id vocabulary-title -->
# Which thing owns which state?

<!-- bmux:block-id vocabulary-map -->
| Concept | Responsibility |
| --- | --- |
| Runtime / session / pane | Execution environment, lifetime, terminal surface |
| Tab / workspace | Plugin-owned attach unit and organization |
| Arrangement | Personal references or deliberately shared organization |
| Client / attachment | Viewer association, local selection and interaction |

<!-- bmux:notes -->
Quick path: Use docs/concepts.md and INVARIANTS.md as vocabulary/ownership authorities. These are conceptual responsibilities, not a new data schema.
Deeper path: A personal reference is not ownership of the underlying process. Removal must preserve discoverability of authorized running work and resolve or suspend an invalid selected input target. A shared edit remains shared; local focus independence is a different claim. Process survival across machine restart is not promised.
<!-- bmux:end-notes -->

---
<!-- bmux:slide-id plugin-contracts -->
<!-- bmux:block-id plugin-contracts-title -->
# Stable contract ≠ runtime implementation

<!-- bmux:block-id plugin-contracts-flow -->
BPDL schema → generated services, clients, events, and capabilities.

<!-- bmux:block-id plugin-contracts-ownership -->
API crates carry stable models and generated transport. Implementations own state, lifecycle, policy, and orchestration. Neutral host primitives do not acquire plugin-domain convenience APIs.

<!-- bmux:notes -->
Quick path: Refer back to tabs-commands and theme-control-v1 in the main deck. Those are exact schema excerpts, not runtime managers.
Deeper path: AGENTS.md requires domain-neutral core and stable plugin API crates. HostRuntimeApi is limited to core_cli_command_run_path, plugin_command_run, storage_get, storage_set, log_write, and recording_write_event. Foundational plugins may call kernel requests; other plugins use their typed services. Do not claim all historical migrations are complete merely because this is the required boundary.
<!-- bmux:end-notes -->

---
<!-- bmux:slide-id image-lifecycle -->
<!-- bmux:block-id image-lifecycle-title -->
# Image resources and raster footprints differ

<!-- bmux:block-id image-lifecycle-table -->
| Host protocol | BMUX strategy | Cleanup distinction |
| --- | --- | --- |
| Kitty | Retain upload; place again | Placement versus content lifetime |
| Sixel / iTerm2 | Cache encoding; emit raster again | Raster footprint repair and repaint |

<!-- bmux:block-id image-lifecycle-contract -->
A failed write or flush makes host state uncertain. Repair assumptions before incremental reuse.

<!-- bmux:notes -->
Quick path: Point to render_attach_frame_inner's pending_kitty_state and failure invalidation, then its successful cache promotion. These are strategy summaries, not full protocol specifications.
Deeper path: The code distinguishes raster_image_protocol footprints from persistent Kitty placements; its comment warns that ED2 can destroy cached Kitty uploads. See docs/images.md and docs/image-performance.md for protocol/performance context. A microbenchmark does not establish correctness on the audience terminal.
Rehearsal gate: confirm supported protocol, exact terminal, resize behavior, and visible cleanup. The tour's rendering script demonstrates text updates, not image protocol acceptance.
<!-- bmux:end-notes -->

---
<!-- bmux:slide-id reusable-ui -->
<!-- bmux:block-id reusable-ui-title -->
# Reuse without reversing dependencies

<!-- bmux:block-id reusable-ui-layers -->
Primitives: geometry, painting, buffers, input, damage.

<!-- bmux:block-id reusable-ui-components -->
Components: reusable controls built on primitives.

<!-- bmux:block-id reusable-ui-runtime -->
Runtime: scheduling and lifecycle built on primitives—not components.

<!-- bmux:block-id reusable-ui-product -->
Applications/plugins: product-specific state and behavior.

<!-- bmux:notes -->
Quick path: This is a dependency/ownership explanation, not a claim that every layer calls the next in a single stack. Source: docs/tui-framework.md and INVARIANTS.md.
Deeper path: Composition still converges on one retained authority. A browser or observer is another transport/consumer, not permission to create competing hit-test, focus, revision, or lifecycle semantics. Embedded rasters explicitly lack some producer logical metadata; do not infer text selection/copy from visible cells.
<!-- bmux:end-notes -->

---
<!-- bmux:slide-id distributed-details -->
<!-- bmux:block-id distributed-title -->
# Optional: recovery is also an ownership problem

<!-- bmux:block-id distributed-flow -->
Durable intent → deterministic consensus application → reconciliation of effects.

<!-- bmux:block-id distributed-contracts -->
- Validate revision, generation, and retry identity against authority.
- Fence obsolete execution owners.
- Do not infer worker death from a lost gateway connection.
- Keep reusable private credentials out of replicated public metadata.

<!-- bmux:notes -->
Quick path: Skip unless the audience asks about distributed operation. This slide explains design contracts from INVARIANTS.md, not a live cluster demonstration or production-readiness claim.
Deeper path: Use plugins/cluster-plugin/src and its tests as a follow-up reading location, not an assertion that this authoring pass inspected or executed every cluster path. A retry identity has a defined deduplication window; it is not eternal exactly-once execution. Reconciliation does not imply rollback of arbitrary external effects. Execution replacement must be explicit and fenced; foreground process survival after host restart is not promised.
Rehearsal gate: any additional distributed demonstration needs separate ownership, failure injection, evidence, and privacy review. None is embedded in this tour.
<!-- bmux:end-notes -->

---
<!-- bmux:slide-id evidence -->
<!-- bmux:block-id evidence-followup-title -->
# Three levels of confidence

<!-- bmux:block-id evidence-levels -->
1. **Contract:** what the system must preserve.
2. **Implementation:** the exact branch or schema we inspected.
3. **Executed evidence:** what this artifact and scenario actually established.

<!-- bmux:block-id evidence-limits -->
Do not promote an unrun demo, a passing screenshot, or a fixture label into a stronger claim.

<!-- bmux:notes -->
Quick path: Return to the sources checklist. Six actual managed scripts passed; that evidence is separate from final audience/privacy rehearsal and browser/Canvas acceptance.
Deeper path: Unit/protocol/pixel assertions, PTY integration, injected failures, and actual terminal/browser rehearsal answer different questions. Benchmark work counts answer another. Retained final output is not an execution-output journal and is not proof all intermediate frames were observed. Redact private artifacts and capability-bearing URLs before sharing.
<!-- bmux:end-notes -->
