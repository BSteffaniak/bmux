# Sources and presentation evidence

Source baseline: checkout `5434a844c46a` plus the teaching rewrite, inspected 2026-10-15. Runtime implementation references were checked in the preceding pass; this pass changes Markdown only. Line ranges are not permanent anchors; recompare excerpts when source changes. Runtime/application code was not changed by this editorial pass.

A **contract** states what must hold; an **implementation excerpt** shows a particular path; **executed evidence** establishes only the checked scenario. All six non-network demos passed fresh managed execution, retained audience snapshots, and intermediate browser/Canvas commits on 2026-10-15. The unchanged Flappy Bird finale separately passed a short live TypeSafe/JEV managed run, with non-placeholder model confidence/latency telemetry, Canvas commit, and presenter exit. This is not a promise of autonomous game skill, complete lifecycle coverage, venue legibility, or physical terminal paint acknowledgment.

Test artifact: Linux x86_64 debug `bmux`, SHA-256 `58895c1b1c38709c2ec779a28ce974eb6a91b368135b8e5cfa35366826025293`, with the existing matching plugin directory and web assets. Private evidence is retained outside the repository. Approval applies to the actual reviewed scripts/artifacts, not arbitrary replacements. Final audience/privacy rehearsal remains required.

## Exact implementation excerpts — main deck and optional appendix

All 15 implementation/DSL excerpts remain preserved across the main deck and appendix and match contiguous literal source text, including indentation. The finder and navigation examples each use separate excerpts. The inline component declaration preserves the actual `demo-evidence` configuration, reformatted over multiple lines for legibility; the closing launch command is an authored usage example, not a production-code quotation. BPDL is labeled `text`.

| Stable block | Exact source and refreshed lines |
| --- | --- |
| `excerpt-split` | [plugins/tabs-plugin-api/bpdl/tabs-plugin.bpdl](../../plugins/tabs-plugin-api/bpdl/tabs-plugin.bpdl), 231–236 |
| `command-route-diagram` | [plugins/tabs-plugin/src/lib.rs](../../plugins/tabs-plugin/src/lib.rs), 412–414 |
| `ownership-map-diagram` | [presentations/bmux-tour/demos/navigation.dsl](demos/navigation.dsl), 17–17 |
| `ownership-shell-check` | [presentations/bmux-tour/demos/navigation.dsl](demos/navigation.dsl), 35–35 |
| `excerpt-catalog` | [plugins/tabs-plugin-api/bpdl/tabs-plugin.bpdl](../../plugins/tabs-plugin-api/bpdl/tabs-plugin.bpdl), 97–101 |
| `excerpt-finder-scope` | [plugins/finder-plugin/src/lib.rs](../../plugins/finder-plugin/src/lib.rs), 370–373 |
| `excerpt-finder-text` | [plugins/finder-plugin/src/lib.rs](../../plugins/finder-plugin/src/lib.rs), 379–383 |
| `excerpt-theme-component` | [plugins/theme-plugin/assets/themes/minimal.toml](../../plugins/theme-plugin/assets/themes/minimal.toml), 21–27 |
| `excerpt-theme-control` | [plugins/theme-plugin/bpdl/theme-plugin.bpdl](../../plugins/theme-plugin/bpdl/theme-plugin.bpdl), 22–28 |
| `excerpt-write-frame` | [packages/attach_pipeline/src/terminal.rs](../../packages/attach_pipeline/src/terminal.rs), 211–214 |
| `excerpt-render-repair` | [packages/cli/src/runtime/attach/runtime.rs](../../packages/cli/src/runtime/attach/runtime.rs), 9508–9516 |
| `excerpt-playbook-check` | [presentations/bmux-tour/demos/evidence.dsl](demos/evidence.dsl), 33–37 |
| `excerpt-managed-admission` | [plugins/playbook-plugin/src/runner.rs](../../plugins/playbook-plugin/src/runner.rs), 206–215 |
| `excerpt-private-notes` | [packages/presenter/src/model.rs](../../packages/presenter/src/model.rs), 75–77 |
| `excerpt-jev-inputs` | [presentations/bmux-tour/demos/jev-flappy.dsl](demos/jev-flappy.dsl), 6–8 |

## Demonstrated scenarios and limits

Acceptance uses [scripts/presenter-tour-acceptance.py](../../scripts/presenter-tour-acceptance.py) with explicit prebuilt artifacts; see [usage](README.md#repeat-acceptance-with-explicit-artifacts). It starts the actual scripts as embedded components in isolated presenters, manually opens/starts them through presenter input, checks managed completion without diagnostics, observes retained HTTP audience snapshots, and checks browser/Canvas commits for intermediate milestones and clean presenter exit. Managed single-authority attach support supplies actual modal key events and shell paste/Enter for every demo. The closed `@input-profile tmux_compat` selector configures only the generated sandbox; real-attach and custom sandbox configuration remain disallowed.

| Component / source | Passed behavior | Limit |
| --- | --- | --- |
| `demo-panes` / [panes.dsl](demos/panes.dsl) | Three panes through split shortcuts, focus navigation, real fixture-listing/kernel/clock output and layout assertion | Commands depend on Linux utilities, not repository test outcomes |
| `demo-navigation` / [navigation.dsl](demos/navigation.dsl) | Original shell PID survives tab switching; private fixture record count, generated workspace and return to existing work | Not a second-attachment or independent-client-focus test; disconnect is not machine restart |
| `demo-finder` / [finder.dsl](demos/finder.dsl) | Actual Find Tab prompt, fuzzy `api` query from a generated second workspace, Enter selects default's API tab | Not exhaustive matching/order coverage; settings can change scope and matching mode |
| `demo-themes` / [themes.dsl](demos/themes.dsl) | Actual Select Theme, Down cycles all bundled presets plus default with two-second previews, Enter confirms Cyberpunk after the cycle, shell continues | Not preview cancellation, revision conflicts, or every layer composition |
| `demo-rendering` / [rendering.dsl](demos/rendering.dsl) | Real `seq` output, scrollback Page Up/Page Down, return to live shell | **Not fault injection**, image protocol acceptance, or universal host compatibility |
| `demo-evidence` / [evidence.dsl](demos/evidence.dsl) | Fixture count plus independent second-pane count/sum, complete-line output/layout assertions, `verified-demo` snapshot | Snapshot does not establish unasserted behavior |

The retained evidence path is private and ephemeral, not a distributable public artifact. Review/redact PTY output, status, capability URLs, and runtime paths before sharing. Do not copy secrets or reusable credentials into ordinary diagnostics or presentation assets.

## Mechanism provenance and scope

These are explanatory models, not generated dependency graphs. The adjacent implementation excerpts remain the exact-source evidence.

| Slide ID | Production source | Scope |
| --- | --- | --- |
| `command-route` | `plugins/tabs-plugin/src/lib.rs`, `split_pane_with_policy` | Target resolution and bounded ratio, not the entire split call path. Plugin policy delegates to typed services. |
| `ownership-map` | `demos/navigation.dsl` | Saved shell PID and fixture check across navigation; not cross-client or host-restart evidence. |
| `preview-lifecycle` | `plugins/theme-plugin/bpdl/theme-plugin.bpdl`, `theme-control-v1` | Successful revision/token-owned service paths; the interactive picker uses a separate internal preview path. Renewal, expiry and failures are not all drawn. |
| `frame-transaction` | `packages/attach_pipeline/src/terminal.rs`; attach `render_attach_frame_inner` | Successful local transport permits presentation-state commit; failed output invalidates incremental assumptions. Physical paint and browser Canvas are separate boundaries. |
| `demo-authority` | Presenter playbook controller, managed display and observer paths | One input owner, read-only output audience, bounded repair. Stop request, cleanup and restart remain distinct lifecycle facts. |

The three diagrams retain their earlier source grounding and now live in optional appendix depth. Fresh Canvas checks cover every one of the 23 main slides at two browser widths; they do not establish venue readability or every native image protocol. Fixed-frame browser width does not itself resize producer terminal geometry. Final rehearsal must deliberately match producer geometry and audience delivery mode.

## Architectural sources and boundaries

- [Concepts](../../docs/concepts.md) and [invariants](../../INVARIANTS.md): execution lifetime versus organization versus attachment-local selection. Personal-reference removal is not resource destruction; deliberate shared edits remain shared. Optional tab bars/decorations do not own authoritative resource creation, destruction, or ordering.
- [Architecture rules](../../AGENTS.md), `tabs-placement-v1`, and `tabs-catalog-v1`: domain-neutral core, typed plugin contracts, plugin-owned orchestration. Stable API crates must not acquire runtime managers or lifecycle behavior.
- Finder `load_windows`, `show_finder`, and `build_entries`: generated catalog client, configured OrderedV1 matching, scope/search projection, and selection are distinct steps.
- [Theme README](../../plugins/theme-plugin/README.md), bundled theme, and [ThemePlugin::reapply_live_theme](../../plugins/theme-plugin/src/lib.rs): left-to-right composition, deep-merged extension maps, separate appearance/component layers, plugin-owned extension interpretation. Schema revision preconditions are stronger claims than a successful preview demo alone.
- `write_frame` and `render_attach_frame_inner`: stage output, deliver, then promote presentation state; partial output invalidates incremental assumptions. Interaction metadata follows committed output. Kitty placement lifetime differs from raster cleanup. See [images](../../docs/images.md) and [image performance](../../docs/image-performance.md).
- [Live components](../../docs/live-components.md), `Owner::prepare`, and [playbook API](../../packages/playbook/README.md): inert parsing, explicit executable approval, source closure freezing, generation-fenced lifecycle, owned cleanup. Isolated paths are not a security sandbox. Managed configuration rejects real-attach, custom sandbox-config, and runtime-selector env overrides.
- [TUI guide](../../docs/tui-framework.md), [responsive delivery](../../docs/responsive-presentations.md), and [AudienceDocument::decode / rejects_private_projection_fields](../../packages/presenter/src/audience.rs): neutral primitives, directional dependencies, bounded public document/resource handling, omitted notes, rejected private projections. Document/frame delivery differ; raster cells do not imply logical selection/copy. HTTP snapshot acceptance is not Canvas rendering acceptance.
- Interactive work and retention remain bounded. Slow-consumer repair must not be described as a complete execution journal. Output repair belongs to its execution/generation and must not apply stale output to a replacement.

## Final operator checklist

- [ ] Approve the immutable executable and matching plugins/worker/observer protocols; record identity/hash. A protocol probe does not authenticate content.
- [ ] Review current script/include bytes and explicitly enable `--allow-playbooks`. Changed executable input requires fresh approval; parsing alone never starts a run.
- [ ] Rehearse manual `r`, `p/n/c`, `s`, stop-on-leave, cleanup acknowledgment, and generation restart on the final setup. A stop request is not completion; Escape only hides the overlay.
- [ ] Confirm actual audience legibility, geometry, focus/navigation, and timing. Choose quick/deeper chapter paths and personal opening wording.
- [ ] Exclude the presenter viewport from screen sharing; verify notes and private source paths are absent from audience output.
- [ ] Perform separate browser/Canvas acceptance before showing a browser audience, with matching native/WASM assets, explicit delivery mode, authentication, and read-only behavior.
- [ ] Keep capability URLs, credentials, observer/runtime paths, and retained evidence private/redacted.

If a final rehearsal fails, disclose the failed scenario and use its implementation slide rather than manufacturing success. The demo passes, exact source comparisons, JEV editorial judgments, and final audience rehearsal answer different questions. See [editorial review](editorial-review.md) for decisions and review limits.

## Teaching rewrite acceptance

The main route now defines BMUX and pane/tab/workspace before using those terms, introduces playbooks before their checks, and explains architecture through the split already demonstrated. Seven interface-heavy slides moved to optional depth; all seven actual live component declarations and configurations are unchanged. All 23 main slides have ready-to-speak SAY, POINT OUT and NEXT notes; all seven demo cards include DO, EXPECTED and IF IT FAILS. [Speaker script](speaker-script.md) and [presenter guide](presenter-guide.md) are companion presenter-only material, not public deck assets.

An independent fresh-context agent reviewed the deck first without code lookup, identified eight concrete teaching/presenter traps, and accepted the corrected route for rehearsal. This is not a newly recruited human audience study. JEV structured comprehension checks supplied a second opinion; no model verdict guarantees a smooth presentation.

All 23 main slides rendered and committed to browser Canvas at 1280×720 and 800×600 using fresh producer geometries of 120×30 and 90×26. Actual venue/terminal/notes layout still requires rehearsal. Component placements were not started in these content checks. The preceding live demo acceptance remains a separate result for the unchanged scripts, not a rerun claimed by these screenshots.
