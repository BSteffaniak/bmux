# Sources and presentation evidence

Source baseline: checkout `f2ddda1bbccf` plus current working-tree implementation, inspected 2026-10-14. Line ranges below were refreshed against current source bytes; they are not permanent anchors. Recompare excerpts when changing artifacts or source.

A **contract** states what must hold; an **implementation excerpt** shows a particular path; **executed evidence** establishes only the scenario actually checked. All six actual embedded demos passed managed execution acceptance, with private evidence at `/tmp/pw-oa5bi7zp`. The harness checks HTTP frames and managed results, **not browser/Canvas acceptance**. Final audience/privacy rehearsal remains required. This evidence does not claim completion of the full project validation suite.

## Exact implementation excerpts

All ten fenced excerpts in the main deck match contiguous literal source text, including indentation. The finder uses two separate excerpts, not one continuous block. BPDL snippets are labeled `text`; architecture arrows are explanatory prose, not quoted code.

| Stable slide / block | Exact source and refreshed lines | Symbol / scope | Supported claim |
| --- | --- | --- | --- |
| `splits-contract` / `excerpt-split` | [tabs-plugin.bpdl](../../plugins/tabs-plugin-api/bpdl/tabs-plugin.bpdl), 231–236 | `tabs-commands.split-pane` | Typed target/direction/ratio and result, not a keymap |
| `ownership-contract` / `excerpt-catalog` | Same schema, 97–101 | `tabs-catalog-v1` | Ordered discovery; contract prohibits rewriting arrangement storage on read |
| `finder-implementation` / `excerpt-finder-scope` | [finder lib.rs](../../plugins/finder-plugin/src/lib.rs), 370–373 | `build_entries` | All/current workspace scope |
| `finder-implementation` / `excerpt-finder-text` | Same file, 379–383 | `build_entries` | Workspace-qualified search text when configured, not fuzzy scoring internals |
| `themes-composition` / `excerpt-theme-component` | [minimal.toml](../../plugins/theme-plugin/assets/themes/minimal.toml), 21–27 | `plugins."bmux.decoration".unfocused` | Real bundled extension data |
| `themes-control` / `excerpt-theme-control` | [theme-plugin.bpdl](../../plugins/theme-plugin/bpdl/theme-plugin.bpdl), 22–28 | `theme-control-v1` | Revisioned selection and token-owned preview |
| `flush-boundary` / `excerpt-write-frame` | [terminal.rs](../../packages/attach_pipeline/src/terminal.rs), 211–214 | `write_frame` | Write and flush must both succeed |
| `rendering-repair` / `excerpt-render-repair` | [attach runtime.rs](../../packages/cli/src/runtime/attach/runtime.rs), 9508–9516 | `render_attach_frame_inner`, write-error branch | Clear row baselines and request full repair |
| `playbooks-boundary` / `excerpt-managed-admission` | [runner.rs](../../plugins/playbook-plugin/src/runner.rs), 196–205 | `Owner::prepare` | Sandbox driver and generated configuration only |
| `presentation-privacy` / `excerpt-private-notes` | [model.rs](../../packages/presenter/src/model.rs), 75–77 | `Slide::notes` | Notes omitted during serialization |

## Demonstrated scenarios and limits

Acceptance uses [scripts/presenter-tour-acceptance.py](../../scripts/presenter-tour-acceptance.py) with explicit prebuilt artifacts; see [usage](README.md#repeat-acceptance-with-explicit-artifacts). It starts the actual scripts as embedded components in isolated presenters, manually opens/starts them through presenter input, checks managed completion without diagnostics, observes retained HTTP audience snapshots, and checks clean presenter exit. Managed single-authority attach support supplies actual modal key events and shell paste/Enter for every demo. The closed `@input-profile tmux_compat` selector configures only the generated sandbox; real-attach and custom sandbox configuration remain disallowed.

| Component / source | Passed behavior | Limit |
| --- | --- | --- |
| `demo-panes` / [panes.dsl](demos/panes.dsl) | Three panes through split shortcuts, focus navigation, real listing/kernel/clock output and layout assertion | Commands depend on Linux utilities, not repository test outcomes |
| `demo-navigation` / [navigation.dsl](demos/navigation.dsl) | Original shell PID survives tab switching; generated workspace and return to existing work | Not a second-attachment or independent-client-focus test; disconnect is not machine restart |
| `demo-finder` / [finder.dsl](demos/finder.dsl) | Actual Find Tab prompt, fuzzy `api` query from a generated second workspace, Enter selects default's API tab | Not exhaustive matching/order coverage; settings can change scope and matching mode |
| `demo-themes` / [themes.dsl](demos/themes.dsl) | Actual Select Theme, Down cycles all bundled presets plus default with two-second previews, Enter confirms Cyberpunk after the cycle, shell continues | Not preview cancellation, revision conflicts, or every layer composition |
| `demo-rendering` / [rendering.dsl](demos/rendering.dsl) | Real `seq` output, scrollback Page Up/Page Down, return to live shell | **Not fault injection**, image protocol acceptance, or universal host compatibility |
| `demo-evidence` / [evidence.dsl](demos/evidence.dsl) | Computed arithmetic results, two-pane layout/screen assertions, `verified-demo` snapshot | Snapshot does not establish unasserted behavior |

The retained evidence path is private and ephemeral, not a distributable public artifact. Review/redact PTY output, status, capability URLs, and runtime paths before sharing. Do not copy secrets or reusable credentials into ordinary diagnostics or presentation assets.

## Diagram provenance and scope

These are explanatory models, not generated dependency graphs. The adjacent implementation excerpts remain the exact-source evidence.

| Slide ID | Source anchors | Deliberate simplification |
| --- | --- | --- |
| `command-route` | `AGENTS.md` core/plugin and foundational-service rules; `plugins/tabs-plugin-api/bpdl/tabs-plugin.bpdl` split command | Logical route only. “Service” means a foundational plugin service; “Host” means generic kernel primitives, not a tabs-specific core API. |
| `ownership-map` | `docs/concepts.md` vocabulary; `INVARIANTS.md` arrangement, optional presentation, and input-target rules | Tab/context is compressed; current session backing is not a universal one-to-one identity. Dashed tab-bar edge is presentation only. |
| `preview-lifecycle` | `plugins/theme-plugin/bpdl/theme-plugin.bpdl` `theme-control-v1` | Revision checked at begin; preview/cancel/confirm use a token. Repeated candidates, renewal, expiry, and failed operations are not drawn. |
| `frame-transaction` | `packages/attach_pipeline/src/terminal.rs` `write_frame`; `packages/cli/src/runtime/attach/runtime.rs` `render_attach_frame_inner` | Pending cursor/graphics promotion versus failed-delivery invalidation. Host pixels cannot be rolled back atomically; browser paint acknowledgment is a separate boundary. |
| `demo-authority` | `plugins/presenter-plugin/src/playbook.rs`; `packages/playbook/src/managed_display.rs`; `plugins/presenter-plugin/src/playbook_capture.rs` and `playbook_observer.rs` | Approval/admission and identity checks are summarized. Latest repair is bounded, not complete history; stop/cleanup/restart are separate lifecycle facts. |

All five diagram slides were separately prepared as PNG assets through the embedded renderer and committed to browser Canvas at 1280×720 and 800×600. This checks real rendering, not venue legibility or every native terminal's image protocol. Speaker notes are explanatory/private, not required labels missing from the audience graph.

## Architectural sources and boundaries

- [Concepts](../../docs/concepts.md) and [invariants](../../INVARIANTS.md): execution lifetime versus organization versus attachment-local selection. Personal-reference removal is not resource destruction; deliberate shared edits remain shared. Optional tab bars/decorations do not own authoritative resource creation, destruction, or ordering.
- [Architecture rules](../../AGENTS.md), `tabs-placement-v1`, and `tabs-catalog-v1`: domain-neutral core, typed plugin contracts, plugin-owned orchestration. Stable API crates must not acquire runtime managers or lifecycle behavior.
- Finder `load_windows`, `show_finder`, and `build_entries`: generated catalog client, configured OrderedV1 matching, scope/search projection, and selection are distinct steps.
- [Theme README](../../plugins/theme-plugin/README.md), bundled theme, and [ThemePlugin::reapply_live_theme](../../plugins/theme-plugin/src/lib.rs): left-to-right composition, deep-merged extension maps, separate appearance/component layers, plugin-owned extension interpretation. Schema revision preconditions are stronger claims than a successful preview demo alone.
- `write_frame` and `render_attach_frame_inner`: stage output, deliver, then promote presentation state; partial output invalidates incremental assumptions. Interaction metadata follows committed output. Kitty placement lifetime differs from raster cleanup. See [images](../../docs/images.md) and [image performance](../../docs/image-performance.md).
- [Live components](../../docs/live-components.md), `Owner::prepare`, and [playbook API](../../packages/playbook/README.md): inert parsing, explicit executable approval, source closure freezing, generation-fenced lifecycle, owned cleanup. Isolated paths are not a security sandbox. Managed configuration rejects real-attach, custom sandbox-config, and runtime-selector env overrides.
- [TUI guide](../../docs/tui-framework.md), [responsive delivery](../../docs/responsive-presentations.md), and [AudienceDocument::decode / rejects_private_projection_fields](../../packages/presenter/src/audience.rs): neutral primitives, directional dependencies, bounded public document/resource handling, omitted notes, rejected private projections. Document/frame delivery differ; raster cells do not imply logical selection/copy. HTTP snapshot acceptance is not Canvas rendering acceptance.
- Interactive work and retention remain bounded. Slow-consumer repair must not be described as a complete execution journal. Output repair belongs to its execution/generation and must not apply stale output to a replacement.
- Optional distributed appendix: [invariants](../../INVARIANTS.md) are the contract authority; [cluster implementation](../../plugins/cluster-plugin/src) is follow-up reading, not a demonstrated deployment. No unlimited deduplication, inferred worker death, arbitrary-effect rollback, or host-restart process-survival promise.

## Final operator checklist

- [ ] Approve the immutable executable and matching plugins/worker/observer protocols; record identity/hash. A protocol probe does not authenticate content.
- [ ] Review current script/include bytes and explicitly enable `--allow-playbooks`. Changed executable input requires fresh approval; parsing alone never starts a run.
- [ ] Rehearse manual `r`, `p/n/c`, `s`, stop-on-leave, cleanup acknowledgment, and generation restart on the final setup. A stop request is not completion; Escape only hides the overlay.
- [ ] Confirm actual audience legibility, geometry, focus/navigation, and timing. Choose quick/deeper chapter paths and personal opening wording.
- [ ] Exclude the presenter viewport from screen sharing; verify notes and private source paths are absent from audience output.
- [ ] Perform separate browser/Canvas acceptance before showing a browser audience, with matching native/WASM assets, explicit delivery mode, authentication, and read-only behavior.
- [ ] Keep capability URLs, credentials, observer/runtime paths, and retained evidence private/redacted.

If a final rehearsal fails, disclose the failed scenario and use its implementation slide rather than manufacturing success. The six-demo pass, exact source comparisons, and final audience rehearsal answer different questions.
